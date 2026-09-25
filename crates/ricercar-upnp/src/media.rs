//! Serving library files and cover art over HTTP.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::Path;

use ricercar_core::meta;

use crate::Renderer;
use crate::http::{self, Request};

pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn pct_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// MIME type for a file extension.
pub fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "m4a" | "mp4" | "alac" | "m4b" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "aif" | "aiff" | "aifc" => "audio/aiff",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

const DLNA_FLAGS: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// 4th protocolInfo field / `contentFeatures.dlna.org` for a MIME type.
pub fn dlna_features(mime: &str) -> String {
    match mime {
        "audio/mpeg" => format!("DLNA.ORG_PN=MP3;{DLNA_FLAGS}"),
        _ => DLNA_FLAGS.to_string(),
    }
}

pub fn protocol_info(mime: &str) -> String {
    format!("http-get:*:{mime}:{}", dlna_features(mime))
}

/// Everything the renderer can decode (ConnectionManager Sink, OpenHome
/// ProtocolInfo).
pub const SINK_PROTOCOLS: &str = "http-get:*:audio/flac:*,http-get:*:audio/x-flac:*,http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,http-get:*:audio/wave:*,http-get:*:audio/aiff:*,http-get:*:audio/x-aiff:*,http-get:*:audio/mpeg:*,http-get:*:audio/mp3:*,http-get:*:audio/mp4:*,http-get:*:audio/x-m4a:*,http-get:*:audio/aac:*,http-get:*:audio/ogg:*,http-get:*:audio/x-ogg:*,http-get:*:application/ogg:*";

/// What the media server hands out (ConnectionManager Source).
pub fn source_protocols() -> String {
    [
        "audio/flac",
        "audio/wav",
        "audio/aiff",
        "audio/mpeg",
        "audio/mp4",
        "audio/aac",
        "audio/ogg",
    ]
    .iter()
    .map(|m| protocol_info(m))
    .collect::<Vec<_>>()
    .join(",")
}

/// URL under which a library file is served.
pub fn media_url(base: &str, file_uri: &str) -> String {
    format!("{base}/media/{}", pct_encode(file_uri))
}

pub fn track_art_url(base: &str, file_uri: &str) -> String {
    format!("{base}/art?u={}", pct_encode(file_uri))
}

pub fn album_art_url(base: &str, album_id: &str) -> String {
    format!("{base}/art?a={}", pct_encode(album_id))
}

enum Range {
    Full,
    Part(u64, u64), // [from, to)
    Unsatisfiable,
}

fn parse_range(h: Option<&str>, len: u64) -> Range {
    let Some(spec) = h.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Range::Full;
    };
    // Multiple ranges are not supported: serve the whole file.
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Range::Full;
    };
    let parsed = if a.is_empty() {
        b.parse::<u64>()
            .ok()
            .map(|n| (len.saturating_sub(n), len))
            .filter(|_| len > 0)
    } else {
        a.parse::<u64>().ok().and_then(|from| {
            let to = if b.is_empty() {
                Some(len)
            } else {
                b.parse::<u64>().ok().map(|t| (t + 1).min(len))
            };
            to.map(|t| (from, t))
        })
    };
    match parsed {
        None if a.is_empty() && len == 0 => Range::Unsatisfiable,
        None => Range::Full,
        Some((from, to)) if from < to && to <= len => Range::Part(from, to),
        Some(_) => Range::Unsatisfiable,
    }
}

/// `GET|HEAD /media/<percent-encoded file URI>`: library files only,
/// streamed from disk with Range support.
pub fn serve_media(r: &Renderer, req: &Request, stream: &mut TcpStream) {
    let head_only = req.method == "HEAD";
    let Some(path) = req
        .route()
        .strip_prefix("/media/")
        .and_then(pct_decode)
        .and_then(|u| meta::uri_to_path(&u))
    else {
        return http::not_found(stream);
    };
    let path_str = path.to_string_lossy().into_owned();
    if !r.ctl.lib.has_path(&path_str) {
        return http::not_found(stream);
    }
    let Ok(mut file) = File::open(&path) else {
        return http::not_found(stream);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mime = mime_of(&path_str);
    let mut headers = vec![
        ("CONTENT-TYPE", mime.to_string()),
        ("ACCEPT-RANGES", "bytes".to_string()),
        ("transferMode.dlna.org", "Streaming".to_string()),
        ("contentFeatures.dlna.org", dlna_features(mime)),
    ];
    let (status, from, to) = match parse_range(req.header("range"), len) {
        Range::Full => (200, 0, len),
        Range::Part(a, b) => {
            headers.push(("CONTENT-RANGE", format!("bytes {a}-{}/{len}", b - 1)));
            (206, a, b)
        }
        Range::Unsatisfiable => {
            http::write_head(
                stream,
                416,
                &[
                    ("CONTENT-RANGE", format!("bytes */{len}")),
                    ("CONTENT-LENGTH", "0".into()),
                ],
            );
            return;
        }
    };
    headers.push(("CONTENT-LENGTH", (to - from).to_string()));
    if !http::write_head(stream, status, &headers) || head_only {
        return;
    }
    if from > 0 && file.seek(SeekFrom::Start(from)).is_err() {
        return;
    }
    // Write timeouts on the socket bound how long a stalled client holds us.
    let _ = std::io::copy(&mut file.take(to - from), stream);
    let _ = stream.flush();
}

/// `GET /art?u=<file uri>` or `/art?a=<album id>`: cover art of library
/// items (or local queue items) only — never fetches remote URLs.
pub fn serve_art(r: &Renderer, req: &Request, stream: &mut TcpStream) {
    let head_only = req.method == "HEAD";
    let lib = &r.ctl.lib;
    // (cache key, track file carrying/next to the art)
    let target: Option<(String, String)> =
        if let Some(a) = req.query("a") {
            pct_decode(a)
                .and_then(|id| lib.album(&id))
                .filter(|al| !al.cover_path.is_empty())
                .map(|al| (al.id, al.cover_path))
        } else if let Some(u) = req.query("u") {
            pct_decode(u).and_then(|uri| {
                let path = meta::uri_to_path(&uri)?.to_string_lossy().into_owned();
                if let Some(t) = lib.track(&path) {
                    return Some((t.album_id, t.path));
                }
                // A local file queued by the UI (not indexed): allowed since it
                // is already in the play queue.
                let queued =
                    r.ctl.lock().queue.iter().any(|q| {
                        q.info.uri == uri && q.info.path.as_deref() == Some(path.as_str())
                    });
                queued.then(|| (path.clone(), path))
            })
        } else {
            None
        };
    let Some((key, track)) = target else {
        return http::not_found(stream);
    };
    let track = Path::new(&track);
    if let Some(thumb) = r.covers().and_then(|c| c.thumb(&key, track, 500))
        && let Ok(bytes) = std::fs::read(&thumb)
    {
        return http::respond(stream, 200, "image/jpeg", &bytes, head_only);
    }
    match meta::cover_bytes(track) {
        Some((bytes, mime)) => http::respond(stream, 200, &mime, &bytes, head_only),
        None => http::not_found(stream),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(h: &str, len: u64) -> Option<(u64, u64)> {
        match parse_range(Some(h), len) {
            Range::Part(a, b) => Some((a, b)),
            _ => None,
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(part("bytes=0-9", 100), Some((0, 10)));
        assert_eq!(part("bytes=90-", 100), Some((90, 100)));
        assert_eq!(part("bytes=-10", 100), Some((90, 100)));
        assert_eq!(part("bytes=50-500", 100), Some((50, 100)));
        assert!(matches!(
            parse_range(Some("bytes=200-"), 100),
            Range::Unsatisfiable
        ));
        assert!(matches!(parse_range(Some("items=1-2"), 100), Range::Full));
        assert!(matches!(parse_range(None, 100), Range::Full));
    }

    #[test]
    fn pct_roundtrip_and_mimes() {
        let s = "file:///a b/c%d.flac";
        assert_eq!(pct_decode(&pct_encode(s)).unwrap(), s);
        assert_eq!(pct_decode("%41%42").unwrap(), "AB");
        assert!(pct_decode("%4").is_none());
        assert_eq!(mime_of("x.FLAC"), "audio/flac");
        assert_eq!(mime_of("x.wav"), "audio/wav");
        assert_eq!(mime_of("x.aiff"), "audio/aiff");
        assert_eq!(mime_of("x.m4a"), "audio/mp4");
        assert_eq!(mime_of("x.opus"), "audio/ogg");
        assert_eq!(mime_of("x.mp3"), "audio/mpeg");
        assert!(protocol_info("audio/mpeg").contains("DLNA.ORG_PN=MP3"));
    }
}
