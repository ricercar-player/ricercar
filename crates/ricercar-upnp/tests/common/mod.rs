#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ricercar_core::{Controller, Library};
use ricercar_upnp::RendererHandle;

pub const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RCS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const CD: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
pub const PLAYLIST: &str = "urn:av-openhome-org:service:Playlist:1";
pub const PRODUCT: &str = "urn:av-openhome-org:service:Product:1";
pub const INFO: &str = "urn:av-openhome-org:service:Info:1";
pub const TIME: &str = "urn:av-openhome-org:service:Time:1";
pub const VOLUME: &str = "urn:av-openhome-org:service:Volume:1";

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn fixture(name: &str) -> String {
    format!("file://{}", fixtures().join(name).display())
}

pub struct Rig {
    pub ctl: Arc<Controller>,
    pub handle: RendererHandle,
    pub out: PathBuf,
}

impl Rig {
    pub fn port(&self) -> u16 {
        self.handle.port
    }
}

/// Renderer on a `file:` sink; `scan` indexes the fixtures first.
pub fn rig(tag: &str, scan: bool) -> Rig {
    let lib = Arc::new(Library::in_memory().unwrap());
    if scan {
        let n = lib.scan_roots(&[fixtures()]).total;
        assert!(n > 0, "scan found nothing");
    }
    let out = std::env::temp_dir().join(format!("ricercar-upnp-{tag}-{}.raw", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let ctl = Arc::new(Controller::new(lib, &format!("file:{}", out.display())));
    let handle = ricercar_upnp::start_renderer(ctl.clone(), "ricercar-test").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    Rig { ctl, handle, out }
}

/// One request, whole response (the server closes the connection).
pub fn raw(port: u16, req: &[u8]) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    (
        String::from_utf8_lossy(&buf[..split]).into_owned(),
        buf[split..].to_vec(),
    )
}

pub fn get(port: u16, path: &str, extra: &str) -> (String, Vec<u8>) {
    raw(
        port,
        format!("GET {path} HTTP/1.1\r\nHOST: 127.0.0.1\r\n{extra}\r\n").as_bytes(),
    )
}

pub fn soap(port: u16, svc_path: &str, ns: &str, action: &str, args: &[(&str, &str)]) -> String {
    let body: String = args
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", ricercar_upnp::xml_escape_pub(v)))
        .collect();
    let envelope = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action} xmlns:u=\"{ns}\">{body}</u:{action}></s:Body></s:Envelope>"
    );
    let req = format!(
        "POST /ctl/{svc_path} HTTP/1.1\r\nHOST: 127.0.0.1:{port}\r\nSOAPAction: \"{ns}#{action}\"\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{envelope}",
        envelope.len()
    );
    let (head, body) = raw(port, req.as_bytes());
    format!("{head}{}", String::from_utf8_lossy(&body))
}

pub fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Unescaped value of an output argument.
pub fn field(resp: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    match resp.split_once(&open) {
        Some((_, rest)) => unescape(rest.split(&close).next().unwrap_or("")),
        None => {
            assert!(
                resp.contains(&format!("<{name}/>")) || resp.contains(&format!("<{name}></")),
                "no <{name}> in {resp}"
            );
            String::new()
        }
    }
}

pub fn fault_code(resp: &str) -> Option<u32> {
    resp.split("<errorCode>")
        .nth(1)?
        .split('<')
        .next()?
        .parse()
        .ok()
}

pub fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

pub fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let mut out = Vec::new();
    for chunk in s.trim().as_bytes().chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let v = chunk.iter().fold(0u32, |acc, c| (acc << 6) | val(*c));
        out.push((v >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((v >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(v as u8);
        }
    }
    out
}

pub fn id_array(resp: &str) -> Vec<u32> {
    b64_decode(&field(resp, "Array"))
        .chunks(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
