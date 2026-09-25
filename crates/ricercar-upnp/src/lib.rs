//! ricercar-upnp — UPnP AV MediaRenderer + OpenHome renderer + MediaServer
//! built on ricercar's engine.
//!
//! One HTTP server hosts two root devices:
//! - a MediaRenderer carrying the UPnP AV services (AVTransport,
//!   RenderingControl, ConnectionManager) and the OpenHome services
//!   (Product, Playlist, Info, Time, Volume) — both views of the same
//!   Controller queue;
//! - a MediaServer (ContentDirectory) browsing the local library.
//!
//! GENA eventing is driven by Controller events (plus a 1 s tick for the
//! playback position and library revision).

mod avt;
mod cds;
pub mod desc;
mod didl;
mod events;
mod http;
mod media;
mod notify;
mod openhome;
mod soap;
mod ssdp;
mod xml;

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ricercar_audio::PcmFormat;
use ricercar_audio::player::TransportStatus;
use ricercar_core::covers::CoverCache;
use ricercar_core::{Controller, QueueItem, Repeat, TrackInfo};

use events::Subscribers;
use soap::{Args, Reply};

/// Exposed for tests and UI integration.
pub fn xml_escape_pub(s: &str) -> String {
    xml::escape(s)
}

/// Concurrent HTTP connections; more are answered 503 and closed.
const MAX_CONNS: usize = 64;

/// Every service we host (renderer and server devices).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Svc {
    Avt,
    Rcs,
    Cms,
    ServerCms,
    Cd,
    OhProduct,
    OhPlaylist,
    OhInfo,
    OhTime,
    OhVolume,
}

impl Svc {
    pub const ALL: [Svc; 10] = [
        Svc::Avt,
        Svc::Rcs,
        Svc::Cms,
        Svc::ServerCms,
        Svc::Cd,
        Svc::OhProduct,
        Svc::OhPlaylist,
        Svc::OhInfo,
        Svc::OhTime,
        Svc::OhVolume,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Svc::Avt => "avt",
            Svc::Rcs => "rcs",
            Svc::Cms => "cms",
            Svc::ServerCms => "scms",
            Svc::Cd => "cd",
            Svc::OhProduct => "ohproduct",
            Svc::OhPlaylist => "ohplaylist",
            Svc::OhInfo => "ohinfo",
            Svc::OhTime => "ohtime",
            Svc::OhVolume => "ohvolume",
        }
    }

    pub fn from_path(p: &str) -> Option<Svc> {
        Svc::ALL.into_iter().find(|s| s.path() == p)
    }

    pub fn urn(self) -> &'static str {
        match self {
            Svc::Avt => "urn:schemas-upnp-org:service:AVTransport:1",
            Svc::Rcs => "urn:schemas-upnp-org:service:RenderingControl:1",
            Svc::Cms | Svc::ServerCms => "urn:schemas-upnp-org:service:ConnectionManager:1",
            Svc::Cd => "urn:schemas-upnp-org:service:ContentDirectory:1",
            Svc::OhProduct => "urn:av-openhome-org:service:Product:1",
            Svc::OhPlaylist => "urn:av-openhome-org:service:Playlist:1",
            Svc::OhInfo => "urn:av-openhome-org:service:Info:1",
            Svc::OhTime => "urn:av-openhome-org:service:Time:1",
            Svc::OhVolume => "urn:av-openhome-org:service:Volume:1",
        }
    }

    pub fn service_id(self) -> &'static str {
        match self {
            Svc::Avt => "urn:upnp-org:serviceId:AVTransport",
            Svc::Rcs => "urn:upnp-org:serviceId:RenderingControl",
            Svc::Cms | Svc::ServerCms => "urn:upnp-org:serviceId:ConnectionManager",
            Svc::Cd => "urn:upnp-org:serviceId:ContentDirectory",
            Svc::OhProduct => "urn:av-openhome-org:serviceId:Product",
            Svc::OhPlaylist => "urn:av-openhome-org:serviceId:Playlist",
            Svc::OhInfo => "urn:av-openhome-org:serviceId:Info",
            Svc::OhTime => "urn:av-openhome-org:serviceId:Time",
            Svc::OhVolume => "urn:av-openhome-org:serviceId:Volume",
        }
    }

    fn index(self) -> usize {
        Svc::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Wake-ups for the event thread.
pub(crate) enum Wake {
    /// A Controller event: the services it may affect, and whether the
    /// queue itself changed.
    Ctl(&'static [Svc], bool),
    /// State kept in this crate changed (stored metadata, standby…).
    Dirty,
    /// Send the initial event to a new subscriber.
    NewSub(Svc, String),
}

/// OpenHome Info counters.
#[derive(Default)]
pub(crate) struct Counters {
    pub track: u32,
    pub details: u32,
    pub metatext: u32,
    last_track: Option<(u64, String)>,
    last_details: Vec<String>,
    last_metatext: String,
}

/// A consistent copy of the controller state for one computation.
pub(crate) struct Snap {
    pub status: TransportStatus,
    pub ids: Vec<u64>,
    pub current: Option<usize>,
    pub cur: Option<QueueItem>,
    pub next: Option<QueueItem>,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub total_ms: u64,
    pub volume: u32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub format: Option<PcmFormat>,
    pub stream_title: Option<String>,
    pub queue_rev: u64,
}

pub(crate) struct Renderer {
    pub ctl: Arc<Controller>,
    pub name: String,
    udn: String,
    server_udn: String,
    /// `ip:port` used in URLs we emit outside of a request (events).
    pub default_host: String,
    subs: Vec<Subscribers>,
    /// Raw DIDL-Lite supplied by control points, by queue item id.
    meta: Mutex<HashMap<u64, String>>,
    pub counters: Mutex<Counters>,
    wake: Mutex<Option<Sender<Wake>>>,
    pub standby: AtomicBool,
    covers: OnceLock<Option<CoverCache>>,
    active_conns: AtomicUsize,
}

impl Renderer {
    pub fn subs(&self, svc: Svc) -> &Subscribers {
        &self.subs[svc.index()]
    }

    pub fn wake(&self, w: Wake) {
        if let Some(tx) = self.wake.lock().unwrap().as_ref() {
            let _ = tx.send(w);
        }
    }

    pub fn covers(&self) -> Option<&CoverCache> {
        self.covers
            .get_or_init(|| Some(CoverCache::new(CoverCache::default_dir())))
            .as_ref()
    }

    pub fn snap(&self) -> Snap {
        let st = self.ctl.lock();
        let cur = st.current_item().cloned();
        let next = st.current.and_then(|i| {
            st.queue.get(i + 1).cloned().or_else(|| {
                (st.repeat == Repeat::All)
                    .then(|| st.queue.first().cloned())
                    .flatten()
            })
        });
        Snap {
            status: st.status,
            ids: st.queue.iter().map(|q| q.id).collect(),
            current: st.current,
            cur,
            next,
            pos_ms: st.pos_ms,
            dur_ms: st.dur_ms,
            total_ms: st.queue.iter().map(|q| q.info.duration_ms).sum(),
            volume: st.volume,
            muted: st.muted,
            shuffle: st.shuffle,
            repeat: st.repeat,
            format: st.chain.format,
            stream_title: st.stream_title.clone(),
            queue_rev: st.queue_rev,
        }
    }

    pub fn store_meta(&self, id: u64, didl: &str) {
        if !didl.trim().is_empty() {
            self.meta.lock().unwrap().insert(id, didl.to_string());
        }
    }

    /// Forget metadata of items no longer queued. The queue is read while
    /// holding the metadata lock, so an item inserted concurrently (queued
    /// first, metadata stored second) is never pruned.
    pub fn prune_meta(&self) {
        let mut m = self.meta.lock().unwrap();
        if !m.is_empty() {
            let keep: std::collections::HashSet<u64> =
                self.ctl.lock().queue.iter().map(|q| q.id).collect();
            m.retain(|k, _| keep.contains(k));
        }
    }

    /// DIDL-Lite for a queue item: the control point's own blob when it gave
    /// one, else generated from what we know.
    pub fn item_meta(&self, item: &QueueItem, base: &str) -> String {
        if let Some(m) = self.meta.lock().unwrap().get(&item.id) {
            return m.clone();
        }
        let info = &item.info;
        let local = info
            .path
            .as_deref()
            .filter(|p| self.ctl.lib.has_path(p))
            .is_some();
        let (res_url, art) = if local {
            (
                media::media_url(base, &info.uri),
                Some(media::track_art_url(base, &info.uri)),
            )
        } else {
            let art = info
                .cover
                .clone()
                .filter(|c| c.starts_with("http://") || c.starts_with("https://"));
            (info.uri.clone(), art)
        };
        let mime = mime_for(info);
        didl::ItemXml {
            id: &oh_id(item.id).to_string(),
            parent: "0",
            info,
            res_url: &res_url,
            protocol_info: &media::protocol_info(mime),
            size: None,
            channels: None,
            bitrate: None,
            art_url: art.as_deref(),
        }
        .document()
    }

    /// Advance the OpenHome Info counters to the given state.
    pub fn update_counters(&self, s: &Snap) -> (u32, u32, u32) {
        let mut c = self.counters.lock().unwrap();
        let track = s.cur.as_ref().map(|q| (q.id, q.info.uri.clone()));
        if track.is_some() && track != c.last_track {
            c.track = c.track.wrapping_add(1);
            c.last_track = track;
        }
        let details = openhome::details(self, s)
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();
        if details != c.last_details {
            c.details = c.details.wrapping_add(1);
            c.last_details = details;
        }
        let meta = s.stream_title.clone().unwrap_or_default();
        if meta != c.last_metatext {
            c.metatext = c.metatext.wrapping_add(1);
            c.last_metatext = meta;
        }
        (c.track, c.details, c.metatext)
    }
}

/// MIME for a queue item: its extension, else its codec.
pub(crate) fn mime_for(info: &TrackInfo) -> &'static str {
    let from_ext = media::mime_of(info.path.as_deref().unwrap_or(&info.uri));
    if from_ext != "application/octet-stream" {
        return from_ext;
    }
    match info.codec.as_deref().unwrap_or("") {
        "FLAC" => "audio/flac",
        "MP3" => "audio/mpeg",
        "WAV" => "audio/wav",
        "AIFF" => "audio/aiff",
        "AAC" => "audio/aac",
        "AAC/ALAC" | "ALAC" => "audio/mp4",
        "Vorbis" | "Opus" => "audio/ogg",
        _ => "audio/*",
    }
}

/// OpenHome ids are ui4: queue ids that don't fit are never exposed.
pub(crate) fn oh_id(id: u64) -> u32 {
    u32::try_from(id).unwrap_or(0)
}

pub struct RendererHandle {
    pub port: u16,
    stop: Arc<AtomicBool>,
    _ssdp: Option<ssdp::Ssdp>,
}

impl RendererHandle {
    pub fn stop(&self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            // Unblock the accept loop.
            let _ = TcpStream::connect_timeout(
                &([127, 0, 0, 1], self.port).into(),
                Duration::from_millis(200),
            );
        }
    }
}

impl Drop for RendererHandle {
    fn drop(&mut self) {
        self.stop();
        // `_ssdp` drops next and sends ssdp:byebye.
    }
}

pub fn start_renderer(controller: Arc<Controller>, name: &str) -> std::io::Result<RendererHandle> {
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let port = listener.local_addr()?.port();
    let udn = load_or_create_udn();
    let mut server_udn = load_or_create_udn_named("udn-server");
    if server_udn == udn {
        // Older versions derived both from the same seed: split them.
        let _ = std::fs::remove_file(config_dir().join("udn-server"));
        server_udn = load_or_create_udn_named("udn-server");
    }
    let ip = ssdp::local_ip();

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Wake>();
    let renderer = Arc::new(Renderer {
        ctl: controller.clone(),
        name: name.to_string(),
        udn: udn.clone(),
        server_udn: server_udn.clone(),
        default_host: format!("{ip}:{port}"),
        subs: Svc::ALL.iter().map(|_| Subscribers::default()).collect(),
        meta: Mutex::new(HashMap::new()),
        counters: Mutex::new(Counters::default()),
        wake: Mutex::new(Some(tx.clone())),
        standby: AtomicBool::new(false),
        covers: OnceLock::new(),
        active_conns: AtomicUsize::new(0),
    });
    renderer.update_counters(&renderer.snap());

    // HTTP service
    {
        let stop_h = stop.clone();
        let r = renderer.clone();
        std::thread::Builder::new()
            .name("ricercar-upnp-http".into())
            .spawn(move || accept_loop(r, listener, stop_h))?;
    }

    // Controller events → event thread.
    {
        let stop_f = stop.clone();
        let ctl_rx = controller.subscribe();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("ricercar-upnp-fwd".into())
            .spawn(move || {
                while !stop_f.load(Ordering::Relaxed) {
                    match ctl_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(ev) => {
                            let queue = ev == ricercar_core::CtlEvent::QueueChanged;
                            if tx.send(Wake::Ctl(notify::affected(&ev), queue)).is_err() {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
    }
    drop(tx);

    {
        let stop_e = stop.clone();
        let r = renderer.clone();
        std::thread::Builder::new()
            .name("ricercar-upnp-evt".into())
            .spawn(move || notify::event_loop(r, rx, stop_e))?;
    }

    let ssdp = ssdp::Ssdp::start(
        port,
        vec![
            ssdp::Device {
                udn,
                path: "/device.xml",
                kind: ssdp::Kind::Renderer,
            },
            ssdp::Device {
                udn: server_udn,
                path: "/server.xml",
                kind: ssdp::Kind::Server,
            },
        ],
    );
    if ssdp.is_none() {
        tracing::warn!("SSDP port 1900 unavailable — renderer not discoverable");
    }

    Ok(RendererHandle {
        port,
        stop,
        _ssdp: ssdp,
    })
}

struct ConnGuard<'a>(&'a AtomicUsize);

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(r: Arc<Renderer>, listener: TcpListener, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(mut stream) = conn else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let _ = stream.set_read_timeout(Some(http::READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(http::WRITE_TIMEOUT));
        if r.active_conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            r.active_conns.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            http::write_response(&mut stream, 503, "text/plain", b"busy");
            continue;
        }
        let r2 = r.clone();
        let spawned = std::thread::Builder::new()
            .name("ricercar-upnp-conn".into())
            .spawn(move || {
                let _guard = ConnGuard(&r2.active_conns);
                serve_conn(&r2, stream);
            });
        if spawned.is_err() {
            r.active_conns.fetch_sub(1, Ordering::SeqCst);
        }
    }
    // Drop the event channel so the event thread ends too.
    r.wake.lock().unwrap().take();
}

fn serve_conn(r: &Renderer, mut stream: TcpStream) {
    let Some(req) = http::read_request(&mut stream) else {
        return;
    };
    let route = req.route().to_string();
    let method = req.method.as_str();
    let xml_type = "text/xml; charset=\"utf-8\"";
    match (method, route.as_str()) {
        ("GET" | "HEAD", "/device.xml") => {
            let xml = desc::device_xml(&r.name, &r.udn);
            http::respond(&mut stream, 200, xml_type, xml.as_bytes(), method == "HEAD");
        }
        ("GET" | "HEAD", "/server.xml") => {
            let xml = desc::server_xml(&r.name, &r.server_udn);
            http::respond(&mut stream, 200, xml_type, xml.as_bytes(), method == "HEAD");
        }
        ("GET" | "HEAD", p) if p.starts_with("/svc/") => {
            let name = p
                .trim_start_matches("/svc/")
                .trim_end_matches(".xml")
                .to_string();
            match Svc::from_path(&name) {
                Some(svc) => http::respond(
                    &mut stream,
                    200,
                    xml_type,
                    desc::scpd_for(svc).as_bytes(),
                    method == "HEAD",
                ),
                None => http::not_found(&mut stream),
            }
        }
        ("GET" | "HEAD", p) if p.starts_with("/media/") => media::serve_media(r, &req, &mut stream),
        ("GET" | "HEAD", "/art") => media::serve_art(r, &req, &mut stream),
        ("POST", p) if p.starts_with("/ctl/") => match Svc::from_path(&p[5..]) {
            Some(svc) => handle_control(r, svc, &req, &mut stream),
            None => http::not_found(&mut stream),
        },
        ("SUBSCRIBE", p) if p.starts_with("/evt/") => match Svc::from_path(&p[5..]) {
            Some(svc) => handle_subscribe(r, svc, &req, &mut stream),
            None => http::not_found(&mut stream),
        },
        ("UNSUBSCRIBE", p) if p.starts_with("/evt/") => match Svc::from_path(&p[5..]) {
            Some(svc) => handle_unsubscribe(r, svc, &req, &mut stream),
            None => http::not_found(&mut stream),
        },
        _ => http::not_found(&mut stream),
    }
}

fn handle_control(r: &Renderer, svc: Svc, req: &http::Request, stream: &mut TcpStream) {
    let (action, args) = match soap::parse(&req.body) {
        Ok(v) => v,
        Err(_) => {
            let f = soap::fault(402, "Invalid Args");
            return http::write_response(stream, 500, "text/xml; charset=\"utf-8\"", f.as_bytes());
        }
    };
    let base = format!("http://{}", req.header("host").unwrap_or(&r.default_host));
    let args = Args(args);
    let reply = match svc {
        Svc::Avt => r.avt(&action, &args, &base),
        Svc::Rcs => r.rcs(&action, &args),
        Svc::Cms => r.cms(&action, false),
        Svc::ServerCms => r.cms(&action, true),
        Svc::Cd => r.cd(&action, &args, &base),
        Svc::OhProduct => r.oh_product(&action, &args),
        Svc::OhPlaylist => r.oh_playlist(&action, &args, &base),
        Svc::OhInfo => r.oh_info(&action, &base),
        Svc::OhTime => r.oh_time(&action),
        Svc::OhVolume => r.oh_volume(&action, &args),
    };
    match reply {
        Reply::Ok(body) => {
            http::write_response(stream, 200, "text/xml; charset=\"utf-8\"", body.as_bytes())
        }
        Reply::Err(code, desc) => http::write_response(
            stream,
            500,
            "text/xml; charset=\"utf-8\"",
            soap::fault(code, desc).as_bytes(),
        ),
    }
}

fn handle_subscribe(r: &Renderer, svc: Svc, req: &http::Request, stream: &mut TcpStream) {
    let timeout: u64 = req
        .header("timeout")
        .and_then(|t| {
            let t = t.trim();
            if t.eq_ignore_ascii_case("infinite") {
                Some(3600)
            } else {
                t.strip_prefix("Second-")
                    .or_else(|| t.strip_prefix("second-"))
                    .and_then(|s| s.parse().ok())
            }
        })
        .unwrap_or(1800)
        .clamp(60, 3600);
    let subs = r.subs(svc);
    let callback = req.header("callback");
    let nt = req.header("nt");
    if let Some(sid) = req.header("sid") {
        if callback.is_some() || nt.is_some() {
            return http::write_response(stream, 400, "text/plain", b"");
        }
        if subs.renew(sid, timeout) {
            write_event_response(stream, sid, timeout);
        } else {
            http::write_response(stream, 412, "text/plain", b"");
        }
        return;
    }
    let callbacks = callback.map(events::parse_callbacks).unwrap_or_default();
    if nt != Some("upnp:event") || callbacks.is_empty() {
        return http::write_response(stream, 412, "text/plain", b"");
    }
    match subs.subscribe(callbacks, timeout) {
        Some(sid) => {
            write_event_response(stream, &sid, timeout);
            r.wake(Wake::NewSub(svc, sid));
        }
        None => http::write_response(stream, 503, "text/plain", b""),
    }
}

fn write_event_response(stream: &mut TcpStream, sid: &str, timeout: u64) {
    http::write_head(
        stream,
        200,
        &[
            ("SID", sid.to_string()),
            ("TIMEOUT", format!("Second-{timeout}")),
            ("CONTENT-LENGTH", "0".into()),
        ],
    );
}

fn handle_unsubscribe(r: &Renderer, svc: Svc, req: &http::Request, stream: &mut TcpStream) {
    let sid = req.header("sid").unwrap_or("");
    let status = if r.subs(svc).unsubscribe(sid) {
        200
    } else {
        412
    };
    http::write_response(stream, status, "text/plain", b"");
}

fn load_or_create_udn() -> String {
    load_or_create_udn_named("udn")
}

fn load_or_create_udn_named(name: &str) -> String {
    let dir = config_dir();
    let file = dir.join(name);
    if let Ok(s) = std::fs::read_to_string(&file) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let host = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let mut seed = format!("{}{name}", host.trim());
    if host.trim().is_empty() {
        let rnd = std::fs::read("/dev/urandom").unwrap_or_default();
        seed = format!("{:?}{name}", &rnd[..16.min(rnd.len())]);
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let udn = format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (hash >> 32) as u32,
        ((hash >> 16) as u16),
        (hash >> 4) as u16 & 0xfff,
        (hash >> 8) as u16 & 0xfff,
        hash & 0xffffffffff
    );
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(&file, &udn).ok();
    udn
}

pub fn config_dir() -> std::path::PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return std::path::PathBuf::from(x).join("ricercar");
    }
    std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/ricercar"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/ricercar"))
}
