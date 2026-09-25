use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ricercar_core::Library;

fn http(port: u16, req: &str, body: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let msg = format!("{req}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(msg.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn raw_get(port: u16, req: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
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

fn result_of(resp: &str) -> String {
    resp.split("<Result>")
        .nth(1)
        .expect("Result field")
        .split("</Result>")
        .next()
        .unwrap()
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn soap_browse(port: u16, object_id: &str) -> String {
    let ns = "urn:schemas-upnp-org:service:ContentDirectory:1";
    let envelope = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"{ns}\"><ObjectID>{object_id}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestCount>0</RequestCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
    );
    http(
        port,
        &format!("POST /ctl/cd HTTP/1.1\r\nHOST: 127.0.0.1\r\nSOAPAction: \"{ns}#Browse\""),
        &envelope,
    )
}

fn rig(tag: &str) -> (ricercar_upnp::RendererHandle, PathBuf) {
    let lib = Arc::new(Library::in_memory().unwrap());
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let n = lib.scan_root(&fixtures);
    assert!(n > 0, "scan found nothing");
    let out = std::env::temp_dir().join(format!("ricercar-dms-{tag}.raw"));
    let _ = std::fs::remove_file(&out);
    let ctl = Arc::new(ricercar_core::Controller::new(
        lib.clone(),
        &format!("file:{}", out.display()),
    ));
    let h = ricercar_upnp::start_renderer(ctl.clone(), "ricercar-dms-test").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    (h, fixtures)
}

#[test]
fn server_desc() {
    let (r, _f) = rig("desc");
    let resp = http(r.port, "GET /server.xml HTTP/1.1\r\nHOST: x\r\n", "");
    assert!(resp.contains("MediaServer"));
    assert!(resp.contains("ContentDirectory"));
    let scpdl = http(r.port, "GET /svc/cd.xml HTTP/1.1\r\nHOST: x\r\n", "");
    assert!(scpdl.contains("Browse"));
}

#[test]
fn browse_and_serve() {
    let (r, fixtures) = rig("browse");

    let root = soap_browse(r.port, "0");
    assert!(root.contains("200 OK"), "{root}");
    let decoded = result_of(&root);
    assert!(decoded.contains("musicAlbum"), "{decoded}");

    let id_start = decoded.find("id=\"A").expect("album id");
    let aid: String = decoded[id_start + 4..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();

    let items = soap_browse(r.port, &aid);
    let body2 = result_of(&items);
    assert!(body2.contains("musicTrack"), "{body2}");

    let mp = body2.find("/media/").expect("media url");
    let pos = body2[..mp].rfind("http://").expect("res url");
    let url: String = body2[pos..].chars().take_while(|c| *c != '<').collect();
    let path_part = url[url.find("/media/").unwrap()..].to_string();

    let (head, bytes) = raw_get(
        r.port,
        &format!("GET {path_part} HTTP/1.1\r\nHOST: x\r\n\r\n"),
    );
    assert!(head.contains("200 OK"), "{head}");

    // the first served item must be one of the fixture files
    let is_fixture = ["tone_16_441.flac", "tone_24_96.flac", "gap_a.flac"]
        .iter()
        .any(|f| bytes == std::fs::read(fixtures.join(f)).unwrap());
    assert!(
        is_fixture,
        "served bytes match no fixture ({} bytes)",
        bytes.len()
    );

    let (head2, bytes2) = raw_get(
        r.port,
        &format!("GET {path_part} HTTP/1.1\r\nHOST: x\r\nRange: bytes=0-9\r\n\r\n"),
    );
    assert!(head2.contains("206 Partial Content"), "{head2}");
    assert!(head2.contains("CONTENT-RANGE: bytes 0-9/"));
    assert_eq!(bytes2.len(), 10);
    assert_eq!(&bytes2[..], &bytes[..10]);
}

#[test]
fn media_rejects_foreign_paths() {
    let (r, _f) = rig("sec");
    let evil = ricercar_upnp_pct("/etc/passwd");
    let (head, _bytes) = raw_get(
        r.port,
        &format!("GET /media/{evil} HTTP/1.1\r\nHOST: x\r\n\r\n"),
    );
    assert!(head.contains("404"), "{head}");
}

fn ricercar_upnp_pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
