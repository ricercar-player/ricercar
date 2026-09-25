use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ricercar_core::Library;
use ricercar_upnp::RendererHandle;

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    format!("file://{}", p.display())
}

fn http(port: u16, req: &str, body: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let msg = format!("{req}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(msg.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn soap(port: u16, path: &str, ns: &str, action: &str, args: &[(&str, &str)]) -> String {
    let body: String = args
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", ricercar_upnp::xml_escape_pub(v)))
        .collect();
    let envelope = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action} xmlns:u=\"{ns}\">{body}</u:{action}></s:Body></s:Envelope>"
    );
    http(
        port,
        &format!("POST {path} HTTP/1.1\r\nHOST: 127.0.0.1\r\nSOAPAction: \"{ns}#{action}\""),
        &envelope,
    )
}

struct TestRig {
    _ctl: Arc<ricercar_core::Controller>,
    handle: RendererHandle,
    out: std::path::PathBuf,
}

fn rig(name: &str) -> TestRig {
    let lib = Arc::new(Library::in_memory().unwrap());
    let out = std::env::temp_dir().join(format!("ricercar-upnp-{name}.raw"));
    let _ = std::fs::remove_file(&out);
    let ctl = Arc::new(ricercar_core::Controller::new(
        lib.clone(),
        &format!("file:{}", out.display()),
    ));
    let handle = ricercar_upnp::start_renderer(ctl.clone(), "ricercar-test").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    TestRig {
        _ctl: ctl,
        handle,
        out,
    }
}

#[test]
fn device_desc() {
    let r = rig("desc");
    let resp = http(r.handle.port, "GET /device.xml HTTP/1.1\r\nHOST: x\r\n", "");
    assert!(resp.contains("MediaRenderer"));
    assert!(resp.contains("AVTransport"));
    assert!(resp.contains("RenderingControl"));
    let scpdl = http(
        r.handle.port,
        "GET /svc/avt.xml HTTP/1.1\r\nHOST: x\r\n",
        "",
    );
    assert!(scpdl.contains("SetNextAVTransportURI"));
}

#[test]
fn transport_flow() {
    let r = rig("flow");
    let ns = "urn:schemas-upnp-org:service:AVTransport:1";
    let didl = "<DIDL-Lite><dc:title>Tone 16</dc:title><upnp:artist>Test</upnp:artist></DIDL-Lite>";
    let resp = soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &fixture("tone_16_441.flac")),
            ("CurrentURIMetaData", didl),
        ],
    );
    assert!(resp.contains("200 OK"), "seturi: {resp}");
    // The file sink is un-paced, so queue the successor before Play.
    soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "SetNextAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("NextURI", &fixture("tone_24_96.flac")),
            ("NextURIMetaData", ""),
        ],
    );
    let resp = soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "Play",
        &[("InstanceID", "0")],
    );
    assert!(resp.contains("200 OK"), "play: {resp}");

    // The file sink is un-paced: the tone may finish in well under a second.
    // Require that at some point the renderer reports PLAYING.
    let mut saw_playing = false;
    for _ in 0..30 {
        let resp = soap(
            r.handle.port,
            "/ctl/avt",
            ns,
            "GetTransportInfo",
            &[("InstanceID", "0")],
        );
        if resp.contains("PLAYING") {
            saw_playing = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(saw_playing, "never reached PLAYING");

    let resp = soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "GetPositionInfo",
        &[("InstanceID", "0")],
    );
    assert!(
        resp.contains("tone_16_441.flac") || resp.contains("tone_24_96.flac"),
        "posinfo: {resp}"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stopped = false;
    while Instant::now() < deadline && !stopped {
        std::thread::sleep(Duration::from_millis(250));
        let info = soap(
            r.handle.port,
            "/ctl/avt",
            ns,
            "GetTransportInfo",
            &[("InstanceID", "0")],
        );
        if info.contains("STOPPED") {
            stopped = true;
        }
    }
    assert!(stopped, "playlist never reached STOPPED");
    let bytes = std::fs::read(&r.out).unwrap_or_default();
    assert!(
        bytes.len() > 1_400_000,
        "expected both tracks in sink, got {} bytes",
        bytes.len()
    );
}

#[test]
fn volume_flow() {
    let r = rig("vol");
    let ns = "urn:schemas-upnp-org:service:RenderingControl:1";
    soap(
        r.handle.port,
        "/ctl/rcs",
        ns,
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", "55"),
        ],
    );
    let resp = soap(
        r.handle.port,
        "/ctl/rcs",
        ns,
        "GetVolume",
        &[("InstanceID", "0"), ("Channel", "Master")],
    );
    assert!(resp.contains("<CurrentVolume>55</CurrentVolume>"), "{resp}");
}

#[test]
fn ssdp_soft_check() {
    let r = rig("ssdp");
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return,
    };
    sock.set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    let msearch = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    let _ = sock.send_to(msearch.as_bytes(), ("127.0.0.1", 1900));
    let mut buf = [0u8; 2048];
    let mut got = false;
    while let Ok((n, _)) = sock.recv_from(&mut buf) {
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        if msg.contains("200 OK") && msg.contains("MediaRenderer") {
            // Loopback requester → loopback LOCATION; UDA 1.1 headers.
            assert!(msg.contains("LOCATION: http://127.0.0.1:"), "{msg}");
            assert!(msg.contains("BOOTID.UPNP.ORG: "), "{msg}");
            assert!(msg.contains("CONFIGID.UPNP.ORG: "), "{msg}");
            assert!(msg.contains("::urn:schemas-upnp-org:device:MediaRenderer:1"));
            got = true;
            break;
        }
    }
    let _ = r.handle;
    if !got {
        eprintln!("note: no SSDP reply (port 1900 may be unavailable in this sandbox)");
    }
}

#[test]
fn http_source_flow() {
    use std::net::TcpListener;
    let r = rig("http");
    let body = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone_16_441.flac"),
    )
    .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let mut stream = stream;
            let mut req = [0u8; 2048];
            let _ = stream.read(&mut req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nCONTENT-TYPE: audio/flac\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_ok() {
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let uri = format!("http://127.0.0.1:{port}/tone.flac");
    let ns = "urn:schemas-upnp-org:service:AVTransport:1";
    let resp = soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &uri),
            ("CurrentURIMetaData", ""),
        ],
    );
    assert!(resp.contains("200 OK"));
    soap(
        r.handle.port,
        "/ctl/avt",
        ns,
        "Play",
        &[("InstanceID", "0")],
    );

    let mut stopped = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !stopped {
        std::thread::sleep(Duration::from_millis(150));
        let info = soap(
            r.handle.port,
            "/ctl/avt",
            ns,
            "GetTransportInfo",
            &[("InstanceID", "0")],
        );
        stopped = info.contains("STOPPED");
    }
    assert!(stopped, "http stream via upnp never finished");
    let bytes = std::fs::read(&r.out).unwrap_or_default();
    assert_eq!(bytes.len(), 352_800, "full 2s tone streamed and written");
}

fn out_arg(resp: &str, k: &str) -> String {
    resp.split(&format!("<{k}>"))
        .nth(1)
        .and_then(|s| s.split(&format!("</{k}>")).next())
        .unwrap_or("")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[test]
fn play_mode_capabilities_and_media_info() {
    let r = rig("modes");
    let p = r.handle.port;
    let ns = "urn:schemas-upnp-org:service:AVTransport:1";
    let iid = [("InstanceID", "0")];
    let caps = soap(p, "/ctl/avt", ns, "GetDeviceCapabilities", &iid);
    assert_eq!(out_arg(&caps, "PlayMedia"), "NETWORK");

    for mode in ["REPEAT_ONE", "REPEAT_ALL", "SHUFFLE", "NORMAL"] {
        let resp = soap(
            p,
            "/ctl/avt",
            ns,
            "SetPlayMode",
            &[("InstanceID", "0"), ("NewPlayMode", mode)],
        );
        assert!(resp.contains("200 OK"), "{mode}: {resp}");
        let s = soap(p, "/ctl/avt", ns, "GetTransportSettings", &iid);
        assert_eq!(out_arg(&s, "PlayMode"), mode);
    }
    let bad = soap(
        p,
        "/ctl/avt",
        ns,
        "SetPlayMode",
        &[("InstanceID", "0"), ("NewPlayMode", "INTRO")],
    );
    assert!(bad.contains("<errorCode>712</errorCode>"), "{bad}");
    // The mapping lands in the Controller.
    soap(
        p,
        "/ctl/avt",
        ns,
        "SetPlayMode",
        &[("InstanceID", "0"), ("NewPlayMode", "REPEAT_ONE")],
    );
    assert_eq!(r._ctl.lock().repeat, ricercar_core::Repeat::One);
    assert!(!r._ctl.lock().shuffle);
    soap(
        p,
        "/ctl/avt",
        ns,
        "SetPlayMode",
        &[("InstanceID", "0"), ("NewPlayMode", "SHUFFLE")],
    );
    assert!(r._ctl.lock().shuffle);
    assert_eq!(r._ctl.lock().repeat, ricercar_core::Repeat::Off);

    let empty = soap(p, "/ctl/avt", ns, "GetMediaInfo", &iid);
    assert_eq!(out_arg(&empty, "NrTracks"), "0");
    assert_eq!(out_arg(&empty, "PlayMedium"), "NONE");

    // DIDL from the control point: parsed into the queue item and handed
    // back verbatim. The URIs cannot play (nothing listens on port 9), so
    // the queue stays put.
    let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>Rock &amp; Roll</dc:title><upnp:artist>Band</upnp:artist><upnp:genre>Rock</upnp:genre><upnp:originalTrackNumber>4</upnp:originalTrackNumber><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:audio/flac:*" duration="0:00:02.000" sampleFrequency="44100" bitsPerSample="16">x</res></item></DIDL-Lite>"#;
    soap(
        p,
        "/ctl/avt",
        ns,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", "http://127.0.0.1:9/one.flac"),
            ("CurrentURIMetaData", didl),
        ],
    );
    // Let the (instant) connection failure settle before queueing more:
    // a failing item is skipped when a successor exists.
    std::thread::sleep(Duration::from_millis(300));
    let media = soap(p, "/ctl/avt", ns, "GetMediaInfo", &iid);
    assert_eq!(out_arg(&media, "NrTracks"), "1");
    assert_eq!(out_arg(&media, "CurrentURI"), "http://127.0.0.1:9/one.flac");
    assert_eq!(out_arg(&media, "CurrentURIMetaData"), didl);
    soap(
        p,
        "/ctl/avt",
        ns,
        "SetNextAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("NextURI", "http://127.0.0.1:9/two.flac"),
            ("NextURIMetaData", ""),
        ],
    );
    let info = r._ctl.lock().queue[0].info.clone();
    assert_eq!(info.title, "Rock & Roll");
    assert_eq!(info.artist.as_deref(), Some("Band"));
    assert_eq!(info.genre.as_deref(), Some("Rock"));
    assert_eq!(info.track_no, Some(4));
    assert_eq!(info.duration_ms, 2_000);
    assert_eq!(info.sample_rate, Some(44_100));
    assert_eq!(info.bits, Some(16));
    let media = soap(p, "/ctl/avt", ns, "GetMediaInfo", &iid);
    assert_eq!(out_arg(&media, "NrTracks"), "2");
    assert_eq!(out_arg(&media, "NextURI"), "http://127.0.0.1:9/two.flac");
    assert!(out_arg(&media, "NextURIMetaData").contains("two.flac"));
}
