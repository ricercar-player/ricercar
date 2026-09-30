use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender as CbSender, TryRecvError};

use crate::device::{DeviceInfo, DeviceKind};
use crate::error::AudioError;
use crate::fmt::PcmFormat;
pub use crate::gain::{GainStage, TrackOpts};
use crate::sink::{AudioSink, device_info, make_sink};
use crate::stream::TrackSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug)]
pub enum EngineCommand {
    Load {
        uri: String,
        opts: TrackOpts,
    },
    EnqueueNext {
        uri: String,
        opts: TrackOpts,
    },
    Pause,
    Resume,
    Stop,
    Seek {
        ms: u64,
    },
    SetVolume {
        percent: u32,
    },
    /// Global preamp in dB, added to each track's own gain.
    SetGain {
        db: f32,
    },
    SetMute {
        muted: bool,
    },
}

/// Why a track stopped playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Played to the end.
    Finished,
    /// `Stop` command.
    Stopped,
    /// Another track was loaded over it.
    Replaced,
    /// Decode or device error.
    Error,
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
        reason: EndReason,
    },
    Position {
        pos_ms: u64,
        dur_ms: Option<u64>,
    },
    /// Title announced in-band by an internet radio (ICY metadata).
    StreamTitle {
        title: String,
    },
    Error {
        message: String,
        /// The source that failed to open, when the error is about one.
        uri: Option<String>,
        /// HTTP status of a failed fetch (401, 403, 404, 410…).
        http_status: Option<u16>,
    },
}

pub struct Subscriber(pub StdReceiver<EngineEvent>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking reader must not take the engine down with it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Minimal fan-out hub (UI, UPnP and MPRIS watch the same engine).
#[derive(Default)]
pub struct EventHub {
    subs: Mutex<Vec<Sender<EngineEvent>>>,
}

impl EventHub {
    pub fn subscribe(&self) -> Subscriber {
        let (tx, rx) = mpsc::channel();
        lock(&self.subs).push(tx);
        Subscriber(rx)
    }
    pub fn publish(&self, ev: EngineEvent) {
        lock(&self.subs).retain(|s| s.send(ev.clone()).is_ok());
    }
}

#[derive(Debug, Clone)]
pub struct ChainInfo {
    pub device: String,
    pub device_kind: DeviceKind,
    pub format: Option<PcmFormat>,
    /// Container actually negotiated with the device (e.g. `S24_3LE`).
    pub container: Option<&'static str>,
    pub volume: u32,
    /// Lossless all the way: native rate, lossless container, unity gain
    /// (volume 100, no ReplayGain/preamp, not muted), and a device kind that
    /// excludes mixing/resampling.
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
    /// Current gain stage (volume, preamp, track gain/peak, mute).
    pub gain: GainStage,
    /// Last in-band stream title of the current track (internet radio).
    pub stream_title: Option<String>,
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
        self.load_with(uri, TrackOpts::default());
    }
    pub fn load_with(&self, uri: impl Into<String>, opts: TrackOpts) {
        self.send(EngineCommand::Load {
            uri: uri.into(),
            opts,
        });
    }
    pub fn enqueue_next(&self, uri: impl Into<String>) {
        self.enqueue_next_with(uri, TrackOpts::default());
    }
    pub fn enqueue_next_with(&self, uri: impl Into<String>, opts: TrackOpts) {
        self.send(EngineCommand::EnqueueNext {
            uri: uri.into(),
            opts,
        });
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
    /// Global preamp in dB (0.0 = off).
    pub fn set_gain(&self, db: f32) {
        self.send(EngineCommand::SetGain { db });
    }
    pub fn set_mute(&self, muted: bool) {
        self.send(EngineCommand::SetMute { muted });
    }
    pub fn subscribe(&self) -> Subscriber {
        self.hub.subscribe()
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Spawn the audio engine on its own thread.
pub fn spawn_player(device_name: &str) -> PlayerHandle {
    spawn_player_with_sink(make_sink(&device_info(device_name)))
}

/// Spawn the engine on a caller-provided sink (embedding, tests).
pub fn spawn_player_with_sink(sink: Box<dyn AudioSink>) -> PlayerHandle {
    let device = sink.device().clone();
    let (tx, rx) = crossbeam_channel::unbounded::<EngineCommand>();
    let hub = Arc::new(EventHub::default());
    let alive = Arc::new(AtomicBool::new(true));
    let gain = GainStage::default();
    let state = Arc::new(Mutex::new(PlayerShared {
        status: TransportStatus::Stopped,
        track_uri: None,
        next_uri: None,
        pos_ms: 0,
        dur_ms: None,
        seekable: false,
        chain: chain_of(&device, None, None, &gain),
        gain,
        stream_title: None,
    }));

    let engine = Engine {
        device,
        sink,
        hub: hub.clone(),
        state: state.clone(),
        status: TransportStatus::Stopped,
        current: None,
        next: None,
        gain,
        pos_frames: 0,
        last_pos_emit: Instant::now(),
    };
    let alive_t = AliveGuard {
        alive: alive.clone(),
        hub: hub.clone(),
        state: state.clone(),
    };
    std::thread::Builder::new()
        .name("ricercar-engine".into())
        .spawn(move || {
            let _alive = alive_t;
            engine.run(rx);
        })
        .expect("spawn engine");

    PlayerHandle {
        tx,
        state,
        hub,
        alive,
    }
}

/// Clears the alive flag however the engine thread exits; a panic that got
/// past the engine's own recovery is reported as a stop with an error.
struct AliveGuard {
    alive: Arc<AtomicBool>,
    hub: Arc<EventHub>,
    state: Arc<Mutex<PlayerShared>>,
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if std::thread::panicking() {
            {
                let mut st = lock(&self.state);
                st.status = TransportStatus::Stopped;
                st.track_uri = None;
                st.next_uri = None;
            }
            self.hub.publish(EngineEvent::Error {
                message: "audio engine stopped after an internal fault".into(),
                uri: None,
                http_status: None,
            });
            self.hub.publish(EngineEvent::Status {
                status: TransportStatus::Stopped,
            });
        }
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Why a track could not be opened, reported when the engine takes it up.
struct OpenFailure {
    message: String,
    uri: Option<String>,
    http_status: Option<u16>,
}

type Opened = std::result::Result<TrackSource, OpenFailure>;

/// Open and prime a track; a panicking demuxer or decoder is an error like
/// any other, not the end of the engine.
fn open_source(uri: &str) -> Opened {
    let opened = catch_unwind(AssertUnwindSafe(|| {
        TrackSource::open(uri).map(|mut src| {
            let ok = prime(&mut src);
            (src, ok)
        })
    }));
    match opened {
        Ok(Ok((src, true))) => Ok(src),
        Ok(Ok((_, false))) => Err(OpenFailure {
            message: format!("cannot decode {uri}"),
            uri: None,
            http_status: None,
        }),
        Ok(Err(e)) => Err(OpenFailure {
            message: format!("{uri}: {e}"),
            uri: Some(uri.to_string()),
            http_status: match &e {
                AudioError::HttpStatus { status, .. } => Some(*status),
                _ => None,
            },
        }),
        Err(p) => Err(OpenFailure {
            message: format!("{uri}: decoder fault: {}", panic_message(p.as_ref())),
            uri: Some(uri.to_string()),
            http_status: None,
        }),
    }
}

fn chain_of(
    device: &DeviceInfo,
    fmt: Option<PcmFormat>,
    container: Option<&'static str>,
    gain: &GainStage,
) -> ChainInfo {
    ChainInfo {
        device: device.name.clone(),
        device_kind: device.kind,
        format: fmt,
        container,
        volume: gain.volume,
        bit_perfect: gain.is_unity() && device.kind.is_bit_perfect(),
    }
}

enum NextSource {
    Pending(String),
    /// Being opened on a worker thread, off the audio path.
    Opening(String, Receiver<Opened>),
    Open(TrackSource),
}

const POSITION_EVERY: Duration = Duration::from_millis(250);

struct Engine {
    device: DeviceInfo,
    sink: Box<dyn AudioSink>,
    hub: Arc<EventHub>,
    state: Arc<Mutex<PlayerShared>>,
    /// Authoritative transport status (mirrored into `state`).
    status: TransportStatus,
    current: Option<TrackSource>,
    next: Option<(NextSource, TrackOpts)>,
    /// `gain.track` holds the options of the current track.
    gain: GainStage,
    /// Frames of the current track handed to the sink.
    pos_frames: u64,
    last_pos_emit: Instant,
}

impl Engine {
    fn run(mut self, rx: Receiver<EngineCommand>) {
        loop {
            match catch_unwind(AssertUnwindSafe(|| self.step(&rx))) {
                Ok(true) => {}
                Ok(false) => return self.shutdown(),
                Err(p) => self.recover(&panic_message(p.as_ref())),
            }
        }
    }

    /// One round of the engine loop; false once the handle is gone.
    fn step(&mut self, rx: &Receiver<EngineCommand>) -> bool {
        if self.status == TransportStatus::Playing {
            // Apply everything queued before producing more audio, so a
            // `load` + `seek`/`pause` burst takes effect atomically.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return false,
                }
            }
            if self.status == TransportStatus::Playing {
                self.pump();
                if self.last_pos_emit.elapsed() >= POSITION_EVERY {
                    self.emit_position();
                }
            }
        } else {
            // Paused / stopped: sleep on the command channel.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        true
    }

    /// Something panicked mid-step: drop the track and go back to a clean
    /// stopped state, ready for the next command.
    fn recover(&mut self, what: &str) {
        self.error(format!("audio engine fault: {what}; playback stopped"));
        self.next = None;
        self.end_current(EndReason::Error);
        self.go_stopped(false);
    }

    fn shutdown(&mut self) {
        let _ = self.sink.discard();
        self.sink.close();
    }

    fn handle(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Load { uri, opts } => self.load(uri, opts),
            EngineCommand::EnqueueNext { uri, opts } => {
                if uri.is_empty() {
                    // Control points clear the next slot this way.
                    self.next = None;
                    lock(&self.state).next_uri = None;
                } else if self.current.is_none() {
                    self.load(uri, opts);
                } else {
                    lock(&self.state).next_uri = Some(uri.clone());
                    self.next = Some((NextSource::Pending(uri), opts));
                }
            }
            EngineCommand::Pause => {
                if self.status == TransportStatus::Playing {
                    self.emit_position();
                    if let Err(e) = self.sink.pause() {
                        return self.sink_failed(e);
                    }
                    self.rewind_dropped();
                    self.set_status(TransportStatus::Paused);
                }
            }
            EngineCommand::Resume => {
                if self.status == TransportStatus::Paused {
                    if let Err(e) = self.sink.resume() {
                        return self.sink_failed(e);
                    }
                    self.set_status(TransportStatus::Playing);
                    self.emit_position();
                }
            }
            EngineCommand::Stop => {
                self.next = None;
                self.end_current(EndReason::Stopped);
                self.go_stopped(false);
            }
            EngineCommand::Seek { ms } => self.seek(ms),
            EngineCommand::SetVolume { percent } => {
                self.gain.volume = percent.min(100);
                self.refresh_chain();
            }
            EngineCommand::SetGain { db } => {
                self.gain.preamp_db = if db.is_finite() { db } else { 0.0 };
                self.refresh_chain();
            }
            EngineCommand::SetMute { muted } => {
                self.gain.muted = muted;
                self.refresh_chain();
            }
        }
    }

    // ------------------------------------------------------------ state

    fn set_status(&mut self, status: TransportStatus) {
        self.status = status;
        lock(&self.state).status = status;
        self.hub.publish(EngineEvent::Status { status });
    }

    fn refresh_chain(&self) {
        let mut st = lock(&self.state);
        let fmt = self.sink.opened_format().or(st.chain.format);
        let container = self
            .sink
            .opened_container()
            .map(|c| c.label())
            .or(st.chain.container);
        st.chain = chain_of(&self.device, fmt, container, &self.gain);
        st.gain = self.gain;
    }

    fn pos_ms(&self) -> u64 {
        let rate = self
            .sink
            .opened_format()
            .or_else(|| self.current.as_ref().and_then(|c| c.format))
            .map_or(44100, |f| f.sample_rate as u64);
        let audible = self.pos_frames.saturating_sub(self.sink.delay_frames());
        audible * 1000 / rate
    }

    fn emit_position(&mut self) {
        self.last_pos_emit = Instant::now();
        let pos_ms = self.pos_ms();
        let dur_ms = {
            let mut st = lock(&self.state);
            st.pos_ms = pos_ms;
            st.dur_ms
        };
        self.hub.publish(EngineEvent::Position { pos_ms, dur_ms });
    }

    fn error(&self, message: String) {
        tracing::warn!("{message}");
        self.hub.publish(EngineEvent::Error {
            message,
            uri: None,
            http_status: None,
        });
    }

    fn end_current(&mut self, reason: EndReason) {
        if let Some(src) = self.current.take() {
            self.hub.publish(EngineEvent::TrackEnded {
                uri: src.uri,
                reason,
            });
        }
    }

    /// Release the device and report Stopped. `drain` lets queued audio play
    /// out (natural end of the queue) instead of cutting it.
    fn go_stopped(&mut self, drain: bool) {
        if drain {
            let _ = self.sink.drain();
        } else {
            let _ = self.sink.discard();
        }
        self.sink.close();
        self.current = None;
        self.pos_frames = 0;
        {
            let mut st = lock(&self.state);
            st.track_uri = None;
            st.next_uri = None;
            st.pos_ms = 0;
            st.dur_ms = None;
            st.seekable = false;
            st.stream_title = None;
        }
        self.set_status(TransportStatus::Stopped);
    }

    /// The device failed under us (e.g. USB DAC unplugged): stop cleanly.
    fn sink_failed(&mut self, e: AudioError) {
        self.error(match &e {
            AudioError::DeviceGone { .. } => format!("{e}; playback stopped"),
            _ => format!("audio output error: {e}"),
        });
        self.next = None;
        self.end_current(EndReason::Error);
        self.sink.close();
        self.go_stopped(false);
    }

    // ------------------------------------------------------------ tracks

    fn open_track(&self, uri: &str) -> Option<TrackSource> {
        self.accept(open_source(uri))
    }

    fn accept(&self, opened: Opened) -> Option<TrackSource> {
        match opened {
            Ok(src) => Some(src),
            Err(f) => {
                tracing::warn!("{}", f.message);
                self.hub.publish(EngineEvent::Error {
                    message: f.message,
                    uri: f.uri,
                    http_status: f.http_status,
                });
                None
            }
        }
    }

    /// Start opening the pending next track on a worker thread (network
    /// sources may take seconds to connect; the device buffer does not).
    fn preopen_next(&mut self) {
        let Some((NextSource::Pending(uri), opts)) = self.next.take() else {
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let u = uri.clone();
        let spawned = std::thread::Builder::new()
            .name("ricercar-preopen".into())
            .spawn(move || {
                let _ = tx.send(open_source(&u));
            });
        self.next = Some(match spawned {
            Ok(_) => (NextSource::Opening(uri, rx), opts),
            Err(_) => (NextSource::Pending(uri), opts),
        });
    }

    /// Take up the pre-opened next track once the worker is done.
    fn poll_next(&mut self) {
        let ready = match &self.next {
            Some((NextSource::Opening(_, rx), _)) => match rx.try_recv() {
                Ok(r) => Some(Some(r)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(None),
            },
            _ => None,
        };
        let Some(result) = ready else { return };
        if let Some((NextSource::Opening(uri, _), opts)) = self.next.take() {
            self.next = match result {
                Some(r) => self.accept(r).map(|t| (NextSource::Open(t), opts)),
                None => Some((NextSource::Pending(uri), opts)),
            };
        }
    }

    /// Without hardware pause the device threw queued audio away: move the
    /// position back to what was heard, and the source with it when it can
    /// seek, so resuming neither skips audio nor misreports the position.
    fn rewind_dropped(&mut self) {
        let dropped = self.sink.take_dropped_frames();
        if dropped == 0 {
            return;
        }
        self.pos_frames = self.pos_frames.saturating_sub(dropped);
        if let Some(src) = self.current.as_mut().filter(|s| s.seekable) {
            let rate = src.format.map_or(44100, |f| f.sample_rate as u64);
            let ms = self.pos_frames * 1000 / rate;
            match catch_unwind(AssertUnwindSafe(|| src.seek_ms(ms))) {
                Ok(Ok(actual)) => {
                    self.pos_frames = (actual.as_secs_f64() * rate as f64).round() as u64;
                }
                Ok(Err(e)) => tracing::warn!("rewind after pause failed: {e}"),
                Err(p) => {
                    tracing::warn!("rewind after pause failed: {}", panic_message(p.as_ref()))
                }
            }
        }
        self.emit_position();
    }

    /// Make sure the sink runs at `fmt`; a fresh open drops what is queued,
    /// otherwise queued audio of the previous format plays out first.
    fn ensure_sink(&mut self, fmt: PcmFormat, fresh: bool) -> bool {
        if !fresh && self.sink.opened_format() == Some(fmt) {
            return true;
        }
        if fresh {
            let _ = self.sink.discard();
        } else {
            let _ = self.sink.drain();
        }
        self.sink.close();
        match self.sink.open(fmt) {
            Ok(()) => true,
            Err(e) => {
                self.error(format!("{}: {e}", self.device.name));
                false
            }
        }
    }

    fn begin(&mut self, src: TrackSource, opts: TrackOpts) {
        let fmt = src.format;
        self.gain.track = opts;
        self.pos_frames = 0;
        {
            let mut st = lock(&self.state);
            st.track_uri = Some(src.uri.clone());
            st.pos_ms = 0;
            st.dur_ms = src.duration_ms;
            st.seekable = src.seekable;
            st.stream_title = None;
        }
        self.refresh_chain();
        self.hub.publish(EngineEvent::TrackStarted {
            uri: src.uri.clone(),
            format: fmt,
        });
        self.current = Some(src);
    }

    fn load(&mut self, uri: String, opts: TrackOpts) {
        self.next = None;
        lock(&self.state).next_uri = None;
        self.end_current(EndReason::Replaced);
        let Some(src) = self.open_track(&uri) else {
            return self.go_stopped(false);
        };
        let Some(fmt) = src.format else {
            return self.go_stopped(false);
        };
        if !self.ensure_sink(fmt, true) {
            return self.go_stopped(false);
        }
        self.begin(src, opts);
        self.set_status(TransportStatus::Playing);
    }

    fn seek(&mut self, ms: u64) {
        let Some(src) = self.current.as_mut().filter(|s| s.seekable) else {
            return;
        };
        let res = catch_unwind(AssertUnwindSafe(|| src.seek_ms(ms))).unwrap_or_else(|p| {
            Err(AudioError::Decode(format!(
                "decoder fault: {}",
                panic_message(p.as_ref())
            )))
        });
        match res {
            Ok(actual) => {
                let rate = src.format.map_or(44100.0, |f| f.sample_rate as f64);
                self.pos_frames = (actual.as_secs_f64() * rate).round() as u64;
                if let Err(e) = self.sink.discard() {
                    return self.sink_failed(e);
                }
                self.emit_position();
            }
            Err(e) => tracing::warn!("seek to {ms} ms failed: {e}"),
        }
    }

    // ------------------------------------------------------------ pumping

    fn pump(&mut self) {
        let Some(src) = self.current.as_mut() else {
            return self.go_stopped(true);
        };

        if let Some(title) = src.take_stream_title() {
            lock(&self.state).stream_title = Some(title.clone());
            self.hub.publish(EngineEvent::StreamTitle { title });
        }

        // Open the next source shortly before the current one ends, so the
        // transition is gapless.
        let near_end = match (src.duration_ms, src.format) {
            (Some(dur), Some(f)) => self.pos_frames * 1000 / f.sample_rate as u64 + 8_000 >= dur,
            _ => false,
        };
        if near_end && matches!(self.next, Some((NextSource::Pending(_), _))) {
            self.preopen_next();
        }
        self.poll_next();

        let Some(src) = self.current.as_mut() else {
            return;
        };
        let pumped = catch_unwind(AssertUnwindSafe(|| src.pump())).unwrap_or_else(|p| {
            Err(AudioError::Decode(format!(
                "decoder fault: {}",
                panic_message(p.as_ref())
            )))
        });
        if let Err(e) = pumped {
            let msg = format!("{}: {e}", src.uri);
            self.error(msg);
            self.end_current(EndReason::Error);
            return self.go_stopped(false);
        }

        if !src.pending.is_empty() {
            let Some(fmt) = src.format else { return };
            // A chained stream may change format mid-track.
            if self.sink.opened_format() != Some(fmt) {
                if !self.ensure_sink(fmt, false) {
                    self.end_current(EndReason::Error);
                    return self.go_stopped(false);
                }
                self.refresh_chain();
            }
            let Some(src) = self.current.as_mut() else {
                return;
            };
            let samples = src.pending.make_contiguous();
            self.gain.apply(samples, fmt.bits);
            let frames = (samples.len() / fmt.channels.max(1) as usize) as u64;
            if let Err(e) = self.sink.write_i32(samples) {
                return self.sink_failed(e);
            }
            src.pending.clear();
            self.pos_frames += frames;
        }

        if self
            .current
            .as_ref()
            .is_some_and(|s| s.end_of_stream && s.pending.is_empty())
        {
            self.transition();
        }
    }

    /// Current track finished: continue gaplessly with the next one, if any.
    fn transition(&mut self) {
        self.end_current(EndReason::Finished);
        lock(&self.state).next_uri = None;
        let next = match self.next.take() {
            Some((NextSource::Open(t), opts)) => Some((t, opts)),
            Some((NextSource::Opening(uri, rx), opts)) => match rx.recv() {
                Ok(r) => self.accept(r),
                Err(_) => self.open_track(&uri),
            }
            .map(|t| (t, opts)),
            Some((NextSource::Pending(uri), opts)) => self.open_track(&uri).map(|t| (t, opts)),
            None => None,
        };
        let Some((src, opts)) = next else {
            return self.go_stopped(true);
        };
        let Some(fmt) = src.format else {
            return self.go_stopped(true);
        };
        // Native-rate switch: flush queued audio, then reopen.
        if !self.ensure_sink(fmt, false) {
            return self.go_stopped(false);
        }
        self.begin(src, opts);
    }
}

/// Decode until the track format is known.
fn prime(src: &mut TrackSource) -> bool {
    if src.format.is_none() && !src.end_of_stream && src.pump().is_err() {
        return false;
    }
    src.format.is_some()
}
