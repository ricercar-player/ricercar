//! Minimal blocking HTTP/1.1 client for media streams: one `GET` per
//! connection, no compression, redirects followed, TLS through rustls.
//!
//! Some CDNs send a hundred header lines or more; the limits here are far
//! above what any real server sends, and only guard memory.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use url::{Position, Url};

use crate::error::{AudioError, Result};
use crate::redact::redact_url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEADERS: usize = 512;
const MAX_HEAD_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 5;

pub type Body = Box<dyn Read + Send>;

pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Body,
    /// The body is chunked: any `Content-Length` is meaningless.
    pub chunked: bool,
}

impl Response {
    /// First value of a header, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `GET uri`, following redirects. Statuses of 400 and above are
/// `AudioError::HttpStatus`; anything else is returned with its body.
pub fn get(uri: &str, range_from: Option<u64>) -> Result<Response> {
    let mut url = Url::parse(uri).map_err(|e| http_err(format!("invalid URL: {e}")))?;
    for _ in 0..=MAX_REDIRECTS {
        let resp = request(&url, range_from)?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.header("location")
        {
            url = redirect_target(&url, loc)?;
            continue;
        }
        if resp.status >= 400 {
            return Err(AudioError::HttpStatus {
                uri: redact_url(uri),
                status: resp.status,
            });
        }
        return Ok(resp);
    }
    Err(http_err(format!("more than {MAX_REDIRECTS} redirects")))
}

fn http_err(msg: String) -> AudioError {
    AudioError::Http(msg)
}

/// Where a redirect leads: relative locations are resolved against the
/// current URL; only http(s) targets, and never from https to http.
pub fn redirect_target(current: &Url, location: &str) -> Result<Url> {
    let next = current
        .join(location.trim())
        .map_err(|e| http_err(format!("invalid redirect location: {e}")))?;
    match (current.scheme(), next.scheme()) {
        ("https", "http") => Err(http_err(format!(
            "refused redirect from https to http ({})",
            redact_url(next.as_str())
        ))),
        (_, "http" | "https") => Ok(next),
        (_, s) => Err(http_err(format!("refused redirect to a {s}: URL"))),
    }
}

fn request(url: &Url, range_from: Option<u64>) -> Result<Response> {
    let host = url
        .host_str()
        .ok_or_else(|| http_err("URL without a host".into()))?
        .to_string();
    let tls = match url.scheme() {
        "https" => true,
        "http" => false,
        s => return Err(http_err(format!("unsupported scheme {s}"))),
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let mut stream = connect(&host, port)?;
    if tls {
        stream = Transport::Tls(Box::new(tls_handshake(stream, &host)?));
    }

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ricercar/{}\r\nAccept: */*\r\n\
         Accept-Encoding: identity\r\nIcy-MetaData: 1\r\n",
        &url[Position::BeforePath..Position::AfterQuery],
        &url[Position::BeforeHost..Position::AfterPort],
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(from) = range_from {
        req.push_str(&format!("Range: bytes={from}-\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| io_err(&host, e))?;

    let mut leftover = Vec::new();
    let (status, headers) = loop {
        let head = read_head(&mut stream, &mut leftover, &host)?;
        let (status, headers) = parse_head(&head, &host)?;
        // Informational responses precede the real one.
        if !(100..200).contains(&status) || status == 101 {
            break (status, headers);
        }
    };
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let chunked = find("transfer-encoding").is_some_and(|te| {
        te.rsplit(',')
            .next()
            .is_some_and(|c| c.trim().eq_ignore_ascii_case("chunked"))
    });
    let length = find("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let raw = BufReader::new(io::Cursor::new(leftover).chain(stream));
    let body: Body = if status == 204 || status == 304 {
        Box::new(io::empty())
    } else if chunked {
        Box::new(Chunked {
            inner: raw,
            left: 0,
            done: false,
        })
    } else if let Some(n) = length {
        Box::new(Limited {
            inner: raw,
            left: n,
        })
    } else {
        Box::new(UntilClose(raw))
    };
    Ok(Response {
        status,
        headers,
        body,
        chunked,
    })
}

fn connect(host: &str, port: u16) -> Result<Transport> {
    let name = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (name, port)
        .to_socket_addrs()
        .map_err(|e| http_err(format!("cannot resolve {host}: {e}")))?
        .collect();
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                s.set_read_timeout(Some(READ_TIMEOUT))?;
                s.set_write_timeout(Some(READ_TIMEOUT))?;
                return Ok(Transport::Plain(s));
            }
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) if is_timeout(&e) => http_err(format!("connection to {host} timed out")),
        Some(e) => http_err(format!("cannot connect to {host}: {e}")),
        None => http_err(format!("cannot resolve {host}: no address")),
    })
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn io_err(host: &str, e: io::Error) -> AudioError {
    if is_timeout(&e) {
        http_err(format!("connection to {host} timed out"))
    } else {
        http_err(format!("connection to {host} failed: {e}"))
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let mut config = rustls::ClientConfig::builder_with_provider(
                rustls::crypto::ring::default_provider().into(),
            )
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("ring supports TLS 1.2 and 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn tls_handshake(stream: Transport, host: &str) -> Result<TlsStream> {
    let Transport::Plain(mut tcp) = stream else {
        unreachable!("TLS over a plain connection only");
    };
    let name = host.trim_start_matches('[').trim_end_matches(']');
    let server = rustls::pki_types::ServerName::try_from(name)
        .map_err(|e| http_err(format!("TLS error with {host}: {e}")))?
        .to_owned();
    let mut conn = rustls::ClientConnection::new(tls_config(), server)
        .map_err(|e| http_err(format!("TLS error with {host}: {e}")))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).map_err(|e| {
            if is_timeout(&e) {
                http_err(format!("connection to {host} timed out"))
            } else {
                http_err(format!("TLS error with {host}: {e}"))
            }
        })?;
    }
    Ok(rustls::StreamOwned::new(conn, tcp))
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

/// Read up to the blank line ending a response head. `buf` holds bytes
/// already received; on return it holds those that follow the head.
fn read_head(stream: &mut Transport, buf: &mut Vec<u8>, host: &str) -> Result<Vec<u8>> {
    let mut scanned = 0;
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(end) = head_end(buf, scanned) {
            let rest = buf.split_off(end);
            return Ok(std::mem::replace(buf, rest));
        }
        scanned = buf.len().saturating_sub(3);
        if buf.len() > MAX_HEAD_BYTES {
            return Err(http_err(format!(
                "response from {host} has a header larger than {} KiB",
                MAX_HEAD_BYTES / 1024
            )));
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(host, e)),
        };
        if n == 0 {
            return Err(http_err(if buf.is_empty() {
                format!("{host} closed the connection without answering")
            } else {
                format!("malformed HTTP response from {host}: incomplete header")
            }));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Offset just past `\r\n\r\n` (or a bare `\n\n`), searching from `from`.
fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    (from..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\r\n\r\n") {
            Some(i + 4)
        } else if buf[i..].starts_with(b"\n\n") {
            Some(i + 2)
        } else {
            None
        }
    })
}

/// Status and headers of a complete head. Shoutcast servers answer with
/// `ICY 200 OK`, taken as HTTP/1.0.
fn parse_head(head: &[u8], host: &str) -> Result<(u16, Vec<(String, String)>)> {
    let fixed;
    let head = match head.strip_prefix(b"ICY ") {
        Some(rest) => {
            fixed = [b"HTTP/1.0 ".as_slice(), rest].concat();
            &fixed[..]
        }
        None => head,
    };
    let mut slots = vec![httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut slots);
    let parsed = httparse::ParserConfig::default()
        .allow_spaces_after_header_name_in_responses(true)
        .allow_obsolete_multiline_headers_in_responses(true)
        .allow_multiple_spaces_in_response_status_delimiters(true)
        .parse_response(&mut resp, head);
    match parsed {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(http_err(format!(
                "malformed HTTP response from {host}: incomplete header"
            )));
        }
        Err(httparse::Error::TooManyHeaders) => {
            let n = head
                .split(|&b| b == b'\n')
                .filter(|l| !l.trim_ascii().is_empty())
                .count()
                .saturating_sub(1);
            return Err(http_err(format!(
                "response from {host} has too many headers ({n})"
            )));
        }
        Err(e) => {
            return Err(http_err(format!(
                "malformed HTTP response from {host}: {e}"
            )));
        }
    }
    let status = resp
        .code
        .ok_or_else(|| http_err(format!("malformed HTTP response from {host}")))?;
    let headers = resp
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).trim().to_string(),
            )
        })
        .collect();
    Ok((status, headers))
}

type Raw = BufReader<io::Chain<io::Cursor<Vec<u8>>, Transport>>;

fn closed_early() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "connection closed before the end of the body",
    )
}

/// A body of known length; ending short of it is an error.
struct Limited {
    inner: Raw,
    left: u64,
}

impl Read for Limited {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let max = (buf.len() as u64).min(self.left) as usize;
        let n = match self.inner.read(&mut buf[..max]) {
            Ok(0) => return Err(closed_early()),
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        self.left -= n as u64;
        Ok(n)
    }
}

/// A body that ends when the server closes the connection. A TLS peer
/// that closes without `close_notify` ends it too.
struct UntilClose(Raw);

impl Read for UntilClose {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            r => r,
        }
    }
}

/// `Transfer-Encoding: chunked`.
struct Chunked {
    inner: Raw,
    /// Bytes left in the current chunk.
    left: u64,
    done: bool,
}

impl Chunked {
    fn line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        (&mut self.inner).take(4096).read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") {
            return Err(if line.len() >= 4096 {
                io::Error::new(io::ErrorKind::InvalidData, "chunk header too long")
            } else {
                closed_early()
            });
        }
        Ok(String::from_utf8_lossy(&line).trim().to_string())
    }
}

impl Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            self.left = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
            if self.left == 0 {
                // Trailers, up to the blank line.
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let max = (buf.len() as u64).min(self.left) as usize;
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(closed_early());
        }
        self.left -= n as u64;
        if self.left == 0 && !self.line()?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing chunk terminator",
            ));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn redirects_resolve_relative_locations() {
        let cur = url("http://a.example/x/y?q=1");
        assert_eq!(
            redirect_target(&cur, "z").unwrap().as_str(),
            "http://a.example/x/z"
        );
        assert_eq!(
            redirect_target(&cur, "/top").unwrap().as_str(),
            "http://a.example/top"
        );
        assert_eq!(
            redirect_target(&cur, "https://b.example/s")
                .unwrap()
                .as_str(),
            "https://b.example/s"
        );
    }

    #[test]
    fn redirect_from_https_to_http_is_refused() {
        let cur = url("https://a.example/x");
        let e = redirect_target(&cur, "http://b.example/y?token=secret").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("refused redirect from https to http"), "{msg}");
        assert!(!msg.contains("secret"), "{msg}");
        assert!(redirect_target(&cur, "file:///etc/passwd").is_err());
        assert!(redirect_target(&cur, "https://b.example/").is_ok());
    }

    #[test]
    fn icy_status_line_and_many_headers_parse() {
        let mut head = b"ICY 200 OK\r\nicy-metaint: 8192\r\n".to_vec();
        for i in 0..150 {
            head.extend(format!("X-Debug: {i}\r\n").as_bytes());
        }
        head.extend(b"\r\n");
        let (status, headers) = parse_head(&head, "h").unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 151);
        assert_eq!(headers[0], ("icy-metaint".into(), "8192".into()));
    }

    #[test]
    fn too_many_headers_and_garbage_are_named() {
        let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..600 {
            head.extend(format!("X-Debug: {i}\r\n").as_bytes());
        }
        head.extend(b"\r\n");
        let e = parse_head(&head, "cdn.example").unwrap_err().to_string();
        assert_eq!(e, "response from cdn.example has too many headers (600)");
        let e = parse_head(b"garbage\r\n\r\n", "cdn.example")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("malformed HTTP response from cdn.example"),
            "{e}"
        );
    }

    /// `bytes` as if received with the head, then a closed connection.
    fn raw(bytes: &[u8]) -> Raw {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        drop(l.accept().unwrap());
        BufReader::new(io::Cursor::new(bytes.to_vec()).chain(Transport::Plain(c)))
    }

    #[test]
    fn chunked_body_is_decoded() {
        let mut body = Chunked {
            inner: raw(b"5;ext=1\r\nhello\r\n7\r\n, world\r\n0\r\nX-T: 1\r\n\r\n"),
            left: 0,
            done: false,
        };
        let mut out = String::new();
        body.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello, world");
    }

    #[test]
    fn short_bodies_are_errors() {
        let mut body = Chunked {
            inner: raw(b"10\r\nshort"),
            left: 0,
            done: false,
        };
        let mut out = Vec::new();
        assert!(body.read_to_end(&mut out).is_err());
        let mut body = Limited {
            inner: raw(b"abc"),
            left: 10,
        };
        let e = body.read_to_end(&mut out).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }
}
