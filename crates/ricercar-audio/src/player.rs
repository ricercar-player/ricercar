use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender as CbSender};

use crate::device::{DeviceInfo, DeviceKind};
use crate::fmt::{Container, PcmFormat};
use crate::sink::device_info;
use crate::sink::{AudioSink, make_sink};
use crate::stream::TrackSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug)]
pub enum EngineCommand {
    Load { uri: String },
    EnqueueNext { uri: String },
    Pause,
    Resume,
    Stop,
    Seek { ms: u64 },
    SetVolume { percent: u32 },
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Status {
        status: TransportStatus,
    },
    TrackStarted {
        uri: String,
        format: Option<PcmFormat>,
    },
    TrackEnded {
        uri: String,
    },
    Position {
        pos_ms: u64,
        dur_ms: Option<u64>,
    },
    Error {
        message: String,
    },
}

pub struct Subscriber(pub StdReceiver<EngineEvent>);

/// Minimal fan-out hub (UI, UPnP and MPRIS watch the same engine).
#[derive(Default)]
pub struct EventHub {
    subs: Mutex<Vec<Sender<EngineEvent>>>,
}

impl EventHub {
    pub fn subscribe(&self) -> Subscriber {
        let (tx, rx) = mpsc::channel();
        self.subs.lock().unwrap().push(tx);
        Subscriber(rx)
    }
    pub fn publish(&self, ev: EngineEvent) {
        let mut dead = Vec::new();
        if let Ok(mut subs) = self.subs.lock() {
            for (i, s) in subs.iter().enumerate() {
                if s.send(ev.clone()).is_err() {
                    dead.push(i);
                }
            }
            for i in dead.into_iter().rev() {
                subs.swap_remove(i);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChainInfo {
    pub device: String,
    pub device_kind: DeviceKind,
    pub format: Option<PcmFormat>,
    pub container: Option<&'static str>,
    pub volume: u32,
    /// Lossless all the way: native rate/bit container, no volume processing,
    /// and a device kind that excludes mixing/resampling.
    pub bit_perfect: bool,
}

#[derive(Debug, Clone)]
pub struct PlayerShared {
    pub status: TransportStatus,
    pub track_uri: Option<String>,
    pub next_uri: Option<String>,
    pub pos_ms: u64,
    pub dur_ms: Option<u64>,
    pub seekable: bool,
    pub chain: ChainInfo,
}

pub struct PlayerHandle {
    tx: CbSender<EngineCommand>,
    pub state: Arc<Mutex<PlayerShared>>,
    pub hub: Arc<EventHub>,
    alive: Arc<AtomicBool>,
}

impl PlayerHandle {
    fn send(&self, cmd: EngineCommand) {
        let _ = self.tx.send(cmd);
    }
    pub fn load(&self, uri: impl Into<String>) {
        self.send(EngineCommand::Load { uri: uri.into() });
    }
    pub fn enqueue_next(&self, uri: impl Into<String>) {
        self.send(EngineCommand::EnqueueNext { uri: uri.into() });
    }
    pub fn pause(&self) {
        self.send(EngineCommand::Pause);
    }
    pub fn resume(&self) {
        self.send(EngineCommand::Resume);
    }
    pub fn stop(&self) {
        self.send(EngineCommand::Stop);
    }
    pub fn seek_ms(&self, ms: u64) {
        self.send(EngineCommand::Seek { ms });
    }
    pub fn set_volume(&self, percent: u32) {
        self.send(EngineCommand::SetVolume {
            percent: percent.min(100),
        });
    }
    pub fn subscribe(&self) -> Subscriber {
        self.hub.subscribe()
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

enum NextSource {
    Pending(String),
    Open(TrackSource),
}

fn chain_of(device: &DeviceInfo, fmt: Option<PcmFormat>, vol: u32) -> ChainInfo {
    ChainInfo {
        device: device.name.clone(),
        device_kind: device.kind,
        format: fmt,
        container: fmt.map(|f| Container::for_bits(f.bits).label()),
        volume: vol,
        bit_perfect: vol == 100 && device.kind.is_bit_perfect(),
    }
}

fn apply_volume(samples: &mut [i32], percent: u32) {
    let vol = percent as i64;
    for s in samples.iter_mut() {
        let v = (*s as i64 * vol + 50) / 100;
        *s = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
}

/// Spawn the audio engine on its own thread.
pub fn spawn_player(device_name: &str) -> PlayerHandle {
    let device = device_info(device_name);
    let (tx, rx) = crossbeam_channel::unbounded::<EngineCommand>();
    let hub = Arc::new(EventHub::default());
    let alive = Arc::new(AtomicBool::new(true));
    let state = Arc::new(Mutex::new(PlayerShared {
        status: TransportStatus::Stopped,
        track_uri: None,
        next_uri: None,
        pos_ms: 0,
        dur_ms: None,
        seekable: false,
        chain: chain_of(&device, None, 100),
    }));

    let hub_t = hub.clone();
    let state_t = state.clone();
    let alive_t = alive.clone();
    std::thread::Builder::new()
        .name("ricercar-engine".into())
        .spawn(move || {
            engine_loop(device, rx, hub_t, state_t);
            alive_t.store(false, Ordering::Relaxed);
        })
        .expect("spawn engine");

    PlayerHandle {
        tx,
        state,
        hub,
        alive,
    }
}

fn open_source(uri: &str, hub: &EventHub) -> Option<TrackSource> {
    match TrackSource::open(uri) {
        Ok(t) => Some(t),
        Err(e) => {
            hub.publish(EngineEvent::Error {
                message: format!("{uri}: {e}"),
            });
            None
        }
    }
}

/// Decode until the track format is known.
fn prime(src: &mut TrackSource) -> bool {
    if src.format.is_none() && !src.end_of_stream && src.pump().is_err() {
        return false;
    }
    src.format.is_some()
}

fn set_status(state: &Mutex<PlayerShared>, hub: &EventHub, status: TransportStatus) {
    state.lock().unwrap().status = status;
    hub.publish(EngineEvent::Status { status });
}

fn stop_engine(sink: &mut Box<dyn AudioSink>, state: &Mutex<PlayerShared>, hub: &EventHub) {
    let _ = sink.drain();
    sink.close();
    {
        let mut st = state.lock().unwrap();
        st.track_uri = None;
        st.pos_ms = 0;
        st.dur_ms = None;
    }
    set_status(state, hub, TransportStatus::Stopped);
}

fn engine_loop(
    device: DeviceInfo,
    rx: Receiver<EngineCommand>,
    hub: Arc<EventHub>,
    state: Arc<Mutex<PlayerShared>>,
) {
    let mut sink = make_sink(&device);
    let mut current: Option<TrackSource> = None;
    let mut next: Option<NextSource> = None;
    let mut volume: u32 = 100;
    let mut pos_frames: u64 = 0u64;

    let mut last_pos_emit = std::time::Instant::now();
    loop {
        // ---- command phase ----
        let cmd = match rx.try_recv() {
            Ok(c) => Some(c),
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            Err(_) => {
                // Idle only when nothing is playing; during playback the pump
                // must run at full speed (a paced pump would starve the sink).
                if state.lock().unwrap().status == TransportStatus::Stopped {
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => None,
                    }
                } else {
                    if last_pos_emit.elapsed() >= Duration::from_millis(250) {
                        last_pos_emit = std::time::Instant::now();
                        let mut st = state.lock().unwrap();
                        let pos = st
                            .chain
                            .format
                            .map(|f| pos_frames * 1000 / f.sample_rate as u64)
                            .unwrap_or(st.pos_ms);
                        st.pos_ms = pos;
                        hub.publish(EngineEvent::Position {
                            pos_ms: pos,
                            dur_ms: st.dur_ms,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    None
                }
            }
        };

        if let Some(cmd) = cmd {
            match cmd {
                EngineCommand::SetVolume { percent } => {
                    volume = percent;
                    let mut st = state.lock().unwrap();
                    st.chain = chain_of(&device, st.chain.format, volume);
                }
                EngineCommand::Load { uri } => {
                    next = None;
                    sink.close();
                    if let Some(mut src) = open_source(&uri, &hub) {
                        if !prime(&mut src) {
                            hub.publish(EngineEvent::Error {
                                message: format!("cannot decode {uri}"),
                            });
                            continue;
                        }
                        let fmt = src.format.unwrap();
                        if let Err(e) = sink.open(fmt) {
                            hub.publish(EngineEvent::Error {
                                message: format!("{uri}: {e}"),
                            });
                            continue;
                        }
                        {
                            let mut st = state.lock().unwrap();
                            st.track_uri = Some(uri.clone());
                            st.next_uri = None;
                            st.pos_ms = 0;
                            st.dur_ms = src.duration_ms;
                            st.seekable = src.seekable;
                            st.chain = chain_of(&device, Some(fmt), volume);
                        }
                        pos_frames = 0;
                        set_status(&state, &hub, TransportStatus::Playing);
                        hub.publish(EngineEvent::TrackStarted {
                            uri,
                            format: Some(fmt),
                        });
                        current = Some(src);
                    }
                }
                EngineCommand::EnqueueNext { uri } => {
                    if uri.is_empty() {
                        // Ignored (e.g. control points clearing the next slot).
                        next = None;
                        state.lock().unwrap().next_uri = None;
                    } else if current.is_none() {
                        // Nothing playing: start it immediately.
                        sink.close();
                        if let Some(mut src) = open_source(&uri, &hub)
                            && prime(&mut src)
                        {
                            let fmt = src.format.unwrap();
                            if sink.open(fmt).is_ok() {
                                {
                                    let mut st = state.lock().unwrap();
                                    st.track_uri = Some(uri.clone());
                                    st.next_uri = None;
                                    st.pos_ms = 0;
                                    st.dur_ms = src.duration_ms;
                                    st.seekable = src.seekable;
                                    st.chain = chain_of(&device, Some(fmt), volume);
                                }
                                pos_frames = 0;
                                set_status(&state, &hub, TransportStatus::Playing);
                                hub.publish(EngineEvent::TrackStarted {
                                    uri,
                                    format: Some(fmt),
                                });
                                current = Some(src);
                            }
                        }
                    } else {
                        next = Some(NextSource::Pending(uri.clone()));
                        state.lock().unwrap().next_uri = Some(uri);
                    }
                }
                EngineCommand::Pause => {
                    let st = state.lock().unwrap().status;
                    if st == TransportStatus::Playing {
                        set_status(&state, &hub, TransportStatus::Paused);
                    }
                }
                EngineCommand::Resume => {
                    let st = state.lock().unwrap().status;
                    if st == TransportStatus::Paused {
                        set_status(&state, &hub, TransportStatus::Playing);
                    }
                }
                EngineCommand::Stop => {
                    current = None;
                    next = None;
                    state.lock().unwrap().next_uri = None;
                    stop_engine(&mut sink, &state, &hub);
                }
                EngineCommand::Seek { ms } => {
                    let mut ok = false;
                    let mut rate = 44100u64;
                    if let Some(src) = &mut current {
                        rate = src.format.map(|f| f.sample_rate as u64).unwrap_or(44100);
                        if src.seekable && src.seek_ms(ms).is_ok() {
                            ok = true;
                        }
                    }
                    if ok {
                        pos_frames = ms * rate / 1000;
                        let st = state.lock().unwrap();
                        hub.publish(EngineEvent::Position {
                            pos_ms: ms,
                            dur_ms: st.dur_ms,
                        });
                    }
                }
            }
        }

        // ---- pump phase ----
        if state.lock().unwrap().status != TransportStatus::Playing {
            continue;
        }

        let mut errored = false;
        let mut ready_for_end = false;

        if let Some(src) = current.as_mut() {
            // Prime the next source shortly before the current one ends.
            let near_end = match src.duration_ms {
                Some(dur) => {
                    let rate = src.format.map(|f| f.sample_rate as u64).unwrap_or(44100);
                    pos_frames * 1000 / rate + 8_000 >= dur
                }
                None => false,
            };
            if near_end
                && matches!(next, Some(NextSource::Pending(_)))
                && let Some(NextSource::Pending(uri)) = next.take()
                && let Some(mut t) = open_source(&uri, &hub)
            {
                prime(&mut t);
                next = Some(NextSource::Open(t));
            }

            match src.pump() {
                Ok(true) => {}
                Ok(false) => {
                    ready_for_end = src.pending.is_empty();
                }
                Err(e) => {
                    errored = true;
                    hub.publish(EngineEvent::Error {
                        message: format!("{}: {e}", src.uri),
                    });
                }
            }

            if !errored {
                let chunk: Vec<i32> = src.pending.drain(..).collect();
                if !chunk.is_empty() {
                    let channels = src.format.map(|f| f.channels as u64).unwrap_or(2);
                    let frames = chunk.len() as u64 / channels;
                    let write = if volume < 100 {
                        let mut c = chunk;
                        apply_volume(&mut c, volume);
                        sink.write_i32(&c)
                    } else {
                        sink.write_i32(&chunk)
                    };
                    match write {
                        Ok(()) => pos_frames += frames,
                        Err(e) => {
                            errored = true;
                            hub.publish(EngineEvent::Error {
                                message: format!("write: {e}"),
                            });
                        }
                    }
                }
            }
        }

        if errored {
            current = None;
            sink.close();
            state.lock().unwrap().track_uri = None;
            set_status(&state, &hub, TransportStatus::Stopped);
            continue;
        }

        if !ready_for_end {
            continue;
        }

        // ---- track transition ----
        let ended = state.lock().unwrap().track_uri.clone().unwrap_or_default();
        hub.publish(EngineEvent::TrackEnded { uri: ended });
        state.lock().unwrap().next_uri = None;

        let next_source = match next.take() {
            Some(NextSource::Open(t)) => Some(t),
            Some(NextSource::Pending(uri)) => open_source(&uri, &hub),
            None => None,
        };

        match next_source {
            Some(mut src2) => {
                if prime(&mut src2) {
                    let new_fmt = src2.format.unwrap();
                    if sink.opened_format() != Some(new_fmt) {
                        // Native-rate switch: flush queued audio, then reopen.
                        let _ = sink.drain();
                        sink.close();
                        match sink.open(new_fmt) {
                            Ok(()) => {
                                let mut st = state.lock().unwrap();
                                st.chain = chain_of(&device, Some(new_fmt), volume);
                            }
                            Err(e) => {
                                hub.publish(EngineEvent::Error {
                                    message: format!("format switch: {e}"),
                                });
                                stop_engine(&mut sink, &state, &hub);
                                current = None;
                                continue;
                            }
                        }
                    }
                    {
                        let mut st = state.lock().unwrap();
                        st.track_uri = Some(src2.uri.clone());
                        st.pos_ms = 0;
                        st.dur_ms = src2.duration_ms;
                        st.seekable = src2.seekable;
                    }
                    pos_frames = 0;
                    hub.publish(EngineEvent::TrackStarted {
                        uri: src2.uri.clone(),
                        format: Some(new_fmt),
                    });
                    current = Some(src2);
                } else {
                    hub.publish(EngineEvent::Error {
                        message: format!("cannot decode {}", src2.uri),
                    });
                    stop_engine(&mut sink, &state, &hub);
                    current = None;
                }
            }
            None => {
                stop_engine(&mut sink, &state, &hub);
                current = None;
            }
        }
    }
    sink.close();
}
