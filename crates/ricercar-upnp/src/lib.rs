//! ricercar-upnp — a UPnP AV MediaRenderer built on ricercar's engine.
//!
//! The device advertises itself over SSDP, serves descriptions over HTTP,
//! and maps AVTransport / RenderingControl / ConnectionManager actions onto
//! the local Controller. Queue state lives in the engine (the renderer owns
//! the play queue, control points only feed it URIs).

pub mod desc;
mod didl;
mod events;
mod http;
mod soap;
mod ssdp;

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ricercar_audio::player::TransportStatus;
use ricercar_core::Controller;

use events::Subscribers;

/// Exposed for tests and UI integration.
pub fn xml_escape_pub(s: &str) -> String {
    desc::xml_escape(s)
}

const AVT_NS: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RCS_NS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CMS_NS: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

#[derive(Default)]
struct Inner {
    current_uri: Option<String>,
    next_uri: Option<String>,
    meta_raw: HashMap<String, String>,
    mute: bool,
    vol_before_mute: u32,
}

pub struct RendererHandle {
    pub port: u16,
    stop: Arc<AtomicBool>,
    _ssdp: Option<ssdp::Ssdp>,
}

impl RendererHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn start_renderer(controller: Arc<Controller>, name: &str) -> std::io::Result<RendererHandle> {
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let port = listener.local_addr()?.port();
    let udn = load_or_create_udn();
    let location = format!("http://{}:{}/device.xml", ssdp::local_ip(), port);

    let stop = Arc::new(AtomicBool::new(false));
    let renderer = Arc::new(Renderer {
        controller,
        inner: RwLock::new(Inner::default()),
        avt_subs: Subscribers::default(),
        rcs_subs: Subscribers::default(),
        cms_subs: Subscribers::default(),
        name: name.to_string(),
        udn: udn.clone(),
    });

    // HTTP service
    {
        let stop_h = stop.clone();
        let r = renderer.clone();
        std::thread::Builder::new()
            .name("ricercar-upnp-http".into())
            .spawn(move || {
                listener.set_nonblocking(true).unwrap();
                while !stop_h.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _peer)) => {
                            let r2 = r.clone();
                            std::thread::spawn(move || serve_conn(&r2, stream));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(30));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            })?;
    }

    // event diff loop (status/uri/volume -> LastChange)
    {
        let stop_e = stop.clone();
        let r = renderer.clone();
        std::thread::Builder::new()
            .name("ricercar-upnp-evt".into())
            .spawn(move || {
                let mut last = r.snapshot();
                loop {
                    if stop_e.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(400));
                    r.avt_subs.expire();
                    r.rcs_subs.expire();
                    r.cms_subs.expire();
                    let snap = r.snapshot();
                    if snap != last {
                        let (state_t, uri, vol, mute) = snap.clone();
                        if r.avt_subs.count() > 0 {
                            let body = avt_last_change(
                                state_t.clone(),
                                uri.clone(),
                                r.meta_raw(uri.as_str()),
                            );
                            r.avt_subs.notify(&body);
                        }
                        if r.rcs_subs.count() > 0 {
                            let body = rcs_last_change(vol, mute);
                            r.rcs_subs.notify(&body);
                        }
                        last = snap;
                    }
                }
            })?;
    }

    let ssdp = ssdp::Ssdp::start(udn, location);
    if ssdp.is_none() {
        tracing::warn!("SSDP port 1900 unavailable — renderer not discoverable");
    }

    Ok(RendererHandle {
        port,
        stop,
        _ssdp: ssdp,
    })
}

struct Renderer {
    controller: Arc<Controller>,
    inner: RwLock<Inner>,
    avt_subs: Subscribers,
    rcs_subs: Subscribers,
    cms_subs: Subscribers,
    name: String,
    udn: String,
}

fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn parse_time(t: &str) -> Option<u64> {
    let mut parts = t.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(((h * 3600 + m * 60) as f64 * 1000.0 + s * 1000.0) as u64)
}

impl Renderer {
    fn snapshot(&self) -> (String, String, u32, bool) {
        let st = self.controller.state.lock().unwrap();
        let transport = match st.status {
            TransportStatus::Playing => "PLAYING",
            TransportStatus::Paused => "PAUSED_PLAYBACK",
            TransportStatus::Stopped => "STOPPED",
        };
        let uri = st.current_uri.clone().unwrap_or_default();
        let mute = self.inner.read().unwrap().mute;
        (transport.to_string(), uri, st.volume, mute)
    }

    fn meta_raw(&self, uri: &str) -> String {
        self.inner
            .read()
            .unwrap()
            .meta_raw
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    fn avt(&self, action: &str, args: Vec<(String, String)>) -> (Option<String>, u16) {
        let arg = |k: &str| {
            args.iter()
                .find(|(a, _)| a == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        match action {
            "SetAVTransportURI" => {
                let uri = arg("CurrentURI");
                let meta = arg("CurrentURIMetaData");
                let info = didl::parse_didl(&uri, &meta);
                {
                    let mut inner = self.inner.write().unwrap();
                    inner.current_uri = Some(uri.clone());
                    inner.next_uri = None;
                    if !meta.is_empty() {
                        inner.meta_raw.insert(uri.clone(), meta);
                    }
                }
                self.controller.set_remote(&uri, info);
                (None, 200)
            }
            "SetNextAVTransportURI" => {
                let uri = arg("NextURI");
                let meta = arg("NextURIMetaData");
                let info = didl::parse_didl(&uri, &meta);
                {
                    let mut inner = self.inner.write().unwrap();
                    inner.next_uri = Some(uri.clone());
                    if !meta.is_empty() {
                        inner.meta_raw.insert(uri.clone(), meta);
                    }
                }
                self.controller.remote_next(&uri, info);
                (None, 200)
            }
            "Play" => {
                let status = self.controller.state.lock().unwrap().status;
                match status {
                    TransportStatus::Paused => self.controller.resume(),
                    TransportStatus::Stopped => {
                        let uri = self.inner.read().unwrap().current_uri.clone();
                        match uri {
                            Some(u) => self.controller.set_remote(&u, None),
                            None => return (Some(soap::fault(8012, "no media")), 500),
                        }
                    }
                    TransportStatus::Playing => {}
                }
                (Some(soap::response_body("Play", AVT_NS, &[])), 200)
            }
            "Pause" => {
                self.controller.pause();
                (Some(soap::response_body("Pause", AVT_NS, &[])), 200)
            }
            "Stop" => {
                self.controller.stop();
                let mut inner = self.inner.write().unwrap();
                inner.current_uri = None;
                inner.next_uri = None;
                (Some(soap::response_body("Stop", AVT_NS, &[])), 200)
            }
            "Seek" => {
                let unit = arg("Unit");
                let target = arg("Target");
                let ms = parse_time(&target).unwrap_or(0);
                if unit != "ABS_TIME" && unit != "REL_TIME" {
                    return (Some(soap::fault(8020, "unsupported seek mode")), 500);
                }
                self.controller.seek_ms(ms);
                (Some(soap::response_body("Seek", AVT_NS, &[])), 200)
            }
            "Next" => {
                self.controller.next();
                (Some(soap::response_body(action, AVT_NS, &[])), 200)
            }
            "Previous" => {
                self.controller.prev();
                (Some(soap::response_body(action, AVT_NS, &[])), 200)
            }
            "SetPlayMode" | "GetTransportSettings" | "GetCrossfadeMode" => {
                (Some(soap::response_body(action, AVT_NS, &[])), 200)
            }
            "GetTransportInfo" => {
                let (t, _, _, _) = self.snapshot();
                (
                    Some(soap::response_body(
                        action,
                        AVT_NS,
                        &[
                            ("CurrentTransportState", t.as_str()),
                            ("CurrentTransportStatus", "OK"),
                            ("CurrentSpeed", "1"),
                        ],
                    )),
                    200,
                )
            }
            "GetPositionInfo" => {
                let st = self.controller.state.lock().unwrap();
                let dur = st.dur_ms;
                let pos = st.pos_ms;
                let track = st.queue_index.map(|i| i + 1).unwrap_or(0);
                let (uri, meta) = st
                    .current_uri
                    .as_ref()
                    .map(|u| {
                        (
                            u.clone(),
                            self.inner
                                .read()
                                .unwrap()
                                .meta_raw
                                .get(u)
                                .cloned()
                                .unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default();
                drop(st);
                (
                    Some(soap::response_body(
                        action,
                        AVT_NS,
                        &[
                            ("Track", &track.to_string()),
                            ("TrackDuration", &fmt_time(dur)),
                            ("TrackMetaData", meta.as_str()),
                            ("TrackURI", uri.as_str()),
                            ("RelTime", &fmt_time(pos)),
                            ("AbsTime", &fmt_time(pos)),
                            ("RelCount", "2147483647"),
                            ("AbsCount", "2147483647"),
                        ],
                    )),
                    200,
                )
            }
            "GetMediaInfo" => {
                let st = self.controller.state.lock().unwrap();
                let cur = st.current_uri.clone().unwrap_or_default();
                let dur = st.dur_ms;
                drop(st);
                let next = self
                    .inner
                    .read()
                    .unwrap()
                    .next_uri
                    .clone()
                    .unwrap_or_default();
                let cur_meta = self.meta_raw(&cur);
                let next_meta = self.meta_raw(&next);
                (
                    Some(soap::response_body(
                        action,
                        AVT_NS,
                        &[
                            ("NrTracks", "1"),
                            ("MediaDuration", &fmt_time(dur)),
                            ("CurrentURI", cur.as_str()),
                            ("CurrentURIMetaData", cur_meta.as_str()),
                            ("NextURI", next.as_str()),
                            ("NextURIMetaData", next_meta.as_str()),
                            ("PlayMedium", "NETWORK"),
                            ("RecordMedium", "NOT_IMPLEMENTED"),
                            ("WriteStatus", "NOT_IMPLEMENTED"),
                        ],
                    )),
                    200,
                )
            }
            "GetDeviceCapabilities" => (
                Some(soap::response_body(
                    action,
                    AVT_NS,
                    &[
                        ("PlayMedia", "0:240p"),
                        ("RecMedia", "NOT_IMPLEMENTED"),
                        ("RecQualityModes", "NOT_IMPLEMENTED"),
                    ],
                )),
                200,
            ),
            "GetCurrentTransportActions" => {
                let (t, _, _, _) = self.snapshot();
                let acts = match t.as_str() {
                    "PLAYING" => "Stop,Pause,Seek,Next,Previous",
                    "PAUSED_PLAYBACK" => "Play,Stop,Seek,Next,Previous",
                    _ => "Play",
                };
                (
                    Some(soap::response_body(action, AVT_NS, &[("Actions", acts)])),
                    200,
                )
            }
            _ => (Some(soap::fault(401, "invalid action")), 500),
        }
    }

    fn rcs(&self, action: &str, args: Vec<(String, String)>) -> (Option<String>, u16) {
        let arg = |k: &str| {
            args.iter()
                .find(|(a, _)| a == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        match action {
            "GetVolume" => {
                let v = self.controller.state.lock().unwrap().volume;
                (
                    Some(soap::response_body(
                        action,
                        RCS_NS,
                        &[("CurrentVolume", &v.to_string())],
                    )),
                    200,
                )
            }
            "SetVolume" => {
                let v = arg("DesiredVolume").parse::<u32>().unwrap_or(100);
                self.controller.set_volume(v);
                (Some(soap::response_body(action, RCS_NS, &[])), 200)
            }
            "GetMute" => {
                let m = self.inner.read().unwrap().mute;
                (
                    Some(soap::response_body(
                        action,
                        RCS_NS,
                        &[("CurrentMute", &(m as i32).to_string())],
                    )),
                    200,
                )
            }
            "SetMute" => {
                let want_mute = arg("DesiredMute") == "1" || arg("DesiredMute") == "true";
                let mut inner = self.inner.write().unwrap();
                if want_mute && !inner.mute {
                    inner.mute = true;
                    let v = self.controller.state.lock().unwrap().volume;
                    inner.vol_before_mute = v;
                    self.controller.set_volume(0);
                } else if !want_mute && inner.mute {
                    inner.mute = false;
                    let v = std::mem::take(&mut inner.vol_before_mute);
                    self.controller.set_volume(v);
                }
                (Some(soap::response_body(action, RCS_NS, &[])), 200)
            }
            _ => (Some(soap::fault(401, "invalid action")), 500),
        }
    }

    fn cms(&self, action: &str) -> (Option<String>, u16) {
        match action {
            "GetProtocolInfo" => (
                Some(soap::response_body(
                    action,
                    CMS_NS,
                    &[
                        ("Source", ""),
                        (
                            "Sink",
                            "http-get:*:flac:*,http-get:*:wav:*,http-get:*:mpeg:*,http-get:*:mp3:*,http-get:*:ogg:*,http-get:*:m4a:*,http-get:*:mp4:*",
                        ),
                    ],
                )),
                200,
            ),
            "GetCurrentConnectionIDs" => (
                Some(soap::response_body(
                    action,
                    CMS_NS,
                    &[("ConnectionIDs", "0")],
                )),
                200,
            ),
            "GetCurrentConnectionInfo" => (
                Some(soap::response_body(
                    action,
                    CMS_NS,
                    &[
                        ("RcsID", "-1"),
                        ("AVTransportID", "-1"),
                        ("ProtocolInfo", ""),
                        ("PeerConnectionManager", ""),
                        ("PeerConnectionID", "-1"),
                        ("Direction", "Input"),
                        ("Status", "OK"),
                    ],
                )),
                200,
            ),
            _ => (Some(soap::fault(401, "invalid action")), 500),
        }
    }
}

fn avt_last_change(transport: String, uri: String, meta: String) -> String {
    format!(
        "<u:LastChange xmlns:u=\"{AVT_NS}\"><InstanceID u:val=\"0\"><TransportState u:val=\"{transport}\"/><CurrentURI u:val=\"{uri}\"/><CurrentURIMetaData u:val=\"{meta}\"/><TransportStatus u:val=\"OK\"/></InstanceID></u:LastChange>"
    )
}

fn rcs_last_change(vol: u32, mute: bool) -> String {
    format!(
        "<u:LastChange xmlns:u=\"{RCS_NS}\"><InstanceID u:val=\"0\"><Volume channel=\"Master\" u:val=\"{vol}\"/><Mute channel=\"Master\" u:val=\"{}\"/></InstanceID></u:LastChange>",
        if mute { 1 } else { 0 }
    )
}

fn serve_conn(r: &Renderer, mut stream: std::net::TcpStream) {
    use http::read_request;
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/device.xml") => {
            let xml = desc::device_xml(&r.name, &r.udn);
            http::write_response(&mut stream, 200, "OK", "text/xml", xml.as_bytes());
        }
        ("GET", "/svc/avt.xml") => http::write_response(
            &mut stream,
            200,
            "OK",
            "text/xml",
            desc::AVT_SCPDL.as_bytes(),
        ),
        ("GET", "/svc/rcs.xml") => http::write_response(
            &mut stream,
            200,
            "OK",
            "text/xml",
            desc::RCS_SCPDL.as_bytes(),
        ),
        ("GET", "/svc/cms.xml") => http::write_response(
            &mut stream,
            200,
            "OK",
            "text/xml",
            desc::CMS_SCPDL.as_bytes(),
        ),
        ("POST", "/ctl/avt") => handle_control(r, AVT_NS, &req, &mut stream),
        ("POST", "/ctl/rcs") => handle_control(r, RCS_NS, &req, &mut stream),
        ("POST", "/ctl/cms") => handle_control(r, CMS_NS, &req, &mut stream),
        ("SUBSCRIBE", p) if p.starts_with("/evt/") => handle_subscribe(r, p, &req, &mut stream),
        ("UNSUBSCRIBE", p) if p.starts_with("/evt/") => handle_unsubscribe(r, p, &req, &mut stream),
        _ => http::write_response(&mut stream, 404, "Not Found", "text/plain", b""),
    }
}

fn subs_of<'a>(r: &'a Renderer, path: &str) -> &'a Subscribers {
    match path.trim_start_matches("/evt/") {
        "rcs" => &r.rcs_subs,
        "cms" => &r.cms_subs,
        _ => &r.avt_subs,
    }
}

fn handle_control(r: &Renderer, ns: &str, req: &http::Request, stream: &mut std::net::TcpStream) {
    let soap_action = req.header("soapaction").unwrap_or("").to_string();
    let ns_owned = soap_action
        .trim_matches('"')
        .rsplit_once('#')
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| ns.to_string());
    match soap::parse(&req.body) {
        Ok((action, args)) => {
            let (body, status) = match ns_owned.as_str() {
                AVT_NS => r.avt(&action, args),
                RCS_NS => r.rcs(&action, args),
                CMS_NS => r.cms(&action),
                _ => (Some(soap::fault(401, "invalid service")), 500),
            };
            match body {
                Some(b) => http::write_response(
                    stream,
                    status,
                    if status == 200 {
                        "OK"
                    } else {
                        "Internal Server Error"
                    },
                    "text/xml; charset=\"utf-8\"",
                    b.as_bytes(),
                ),
                None => http::write_response(
                    stream,
                    200,
                    "OK",
                    "text/xml; charset=\"utf-8\"",
                    empty_response(&action, &ns_owned).as_bytes(),
                ),
            }
        }
        Err(_) => {
            let f = soap::fault(400, "bad request");
            http::write_response(
                stream,
                500,
                "Internal Server Error",
                "text/xml",
                f.as_bytes(),
            );
        }
    }
}

fn empty_response(action: &str, ns: &str) -> String {
    soap::response_body(action, ns, &[])
}

fn handle_subscribe(
    r: &Renderer,
    path: &str,
    req: &http::Request,
    stream: &mut std::net::TcpStream,
) {
    let callback = req.header("callback").unwrap_or("");
    let nts = req.header("nts").unwrap_or("upnp:event");
    let timeout: u64 = req
        .header("timeout")
        .and_then(|t| t.strip_prefix("Second-"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800);
    let timeout = timeout.clamp(60, 3600);
    let subs = subs_of(r, path);
    if nts.contains("renew") {
        if let Some(sid) = req.header("sid")
            && subs.renew(sid, timeout)
        {
            write_event_response(stream, sid, timeout);
            return;
        }
        http::write_response(stream, 412, "Precondition Failed", "text/plain", b"");
    } else {
        let cb = callback.trim_matches('<').trim_end_matches('>');
        if !cb.starts_with("http://") {
            http::write_response(stream, 412, "Precondition Failed", "text/plain", b"");
            return;
        }
        let (sid, t) = subs.subscribe(cb, timeout);
        write_event_response(stream, &sid, t);
    }
}

fn write_event_response(stream: &mut std::net::TcpStream, sid: &str, timeout: u64) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nSID: {sid}\r\nTIMEOUT: Second-{timeout}\r\nSERVER: Linux/5.0 UPnP/1.1 ricercar/0.1\r\nCONTENT-LENGTH: 0\r\nCONNECTION: close\r\n\r\n"
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

fn handle_unsubscribe(
    r: &Renderer,
    path: &str,
    req: &http::Request,
    stream: &mut std::net::TcpStream,
) {
    let sid = req.header("sid").unwrap_or("");
    let ok = subs_of(r, path).unsubscribe(sid);
    let status = if ok { 200 } else { 412 };
    http::write_response(
        stream,
        status,
        if ok { "OK" } else { "Precondition Failed" },
        "text/plain",
        b"",
    );
}

fn load_or_create_udn() -> String {
    let dir = config_dir();
    let file = dir.join("udn");
    if let Ok(s) = std::fs::read_to_string(&file) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let host = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let mut seed = host.trim().to_string();
    if seed.is_empty() {
        let rnd = std::fs::read("/dev/urandom").unwrap_or_default();
        seed = format!("{:?}", &rnd[..16.min(rnd.len())]);
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
