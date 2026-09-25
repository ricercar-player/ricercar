//! Minimal HTTP/1.1 server side: one request per connection
//! (`Connection: close`), bounded header/body sizes, timeouts.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub const SERVER: &str = "Linux/6 UPnP/1.1 ricercar/0.3";
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
/// Time allowed for a client to send its request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A client that accepts no data for this long is dropped.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Path without the query string.
    pub fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    /// Value of a query parameter (still percent-encoded).
    pub fn query(&self, key: &str) -> Option<&str> {
        let q = self.path.split_once('?')?.1;
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HEAD {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if content_len > MAX_BODY {
        return None;
    }
    let mut body: Vec<u8> = buf[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Status line + headers (Content-Length is the caller's job via `extra`
/// or `write_response`).
pub fn write_head(stream: &mut TcpStream, status: u16, extra: &[(&str, String)]) -> bool {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nSERVER: {SERVER}\r\nCONNECTION: close\r\n",
        reason(status)
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).is_ok()
}

pub fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    respond(stream, status, content_type, body, false);
}

/// Full response; `head_only` for HEAD requests.
pub fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) {
    if write_head(
        stream,
        status,
        &[
            ("CONTENT-TYPE", content_type.to_string()),
            ("CONTENT-LENGTH", body.len().to_string()),
        ],
    ) && !head_only
    {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

pub fn not_found(stream: &mut TcpStream) {
    write_response(stream, 404, "text/plain", b"");
}
