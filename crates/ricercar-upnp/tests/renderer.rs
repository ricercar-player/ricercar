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
    let scpdl = http(r.handle.port, "GET /svc/avt.xml HTTP/1.1\r\nHOST: x\r\n", "");
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
    let resp = soap(r.handle.port, "/ctl/avt", ns, "Play", &[("InstanceID", "0")]);
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stopped = false;
    while Instant::now() < deadline && !stopped {
        std::thread::sleep(Duration::from_millis(250));
        let info = soap(r.handle.port, "/ctl/avt", ns, "GetTransportInfo", &[("InstanceID", "0")]);
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
        &[("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", "55")],
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
    sock.set_read_timeout(Some(Duration::from_millis(800))).unwrap();
    let msearch = "M-SEARCH * HTTP/1.1\r\nHOST: 239.2.55.52:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    let _ = sock.send_to(msearch.as_bytes(), ("127.0.0.1", 1900));
    let mut buf = [0u8; 2048];
    let mut got = false;
    while let Ok((n, _)) = sock.recv_from(&mut buf) {
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        if msg.contains("200 OK") && msg.contains("MediaRenderer") {
            got = true;
            break;
        }
    }
    let _ = r.handle;
    if !got {
        eprintln!("note: no SSDP reply (port 1900 may be unavailable in this sandbox)");
    }
}
