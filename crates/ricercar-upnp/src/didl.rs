//! DIDL-Lite: parse control-point metadata into `TrackInfo`, and generate
//! item metadata for items we describe ourselves.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use ricercar_core::TrackInfo;

use crate::xml;

pub const DIDL_OPEN: &str = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">";
pub const DIDL_CLOSE: &str = "</DIDL-Lite>";

/// `H+:MM:SS[.fff]` or `H+:MM:SS.F0/F1` → milliseconds.
pub fn parse_duration(t: &str) -> Option<u64> {
    let t = t.trim();
    let mut parts = t.splitn(3, ':');
    let h: u64 = parts.next()?.trim_start_matches('+').parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    let (sec, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let s: u64 = sec.parse().ok()?;
    let frac_ms = if let Some((a, b)) = frac.split_once('/') {
        let a: f64 = a.parse().ok()?;
        let b: f64 = b.parse().ok()?;
        if b > 0.0 { (a / b * 1000.0) as u64 } else { 0 }
    } else if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let v: u64 = digits.parse().ok()?;
        v * 10u64.pow(3 - digits.len() as u32)
    };
    if m >= 60 || s >= 60 {
        return None;
    }
    Some((h * 3600 + m * 60 + s) * 1000 + frac_ms)
}

/// `H:MM:SS.fff` as used by DIDL `res@duration`.
pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    format!(
        "{}:{:02}:{:02}.{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

/// Codec name from a MIME type in a protocolInfo (`http-get:*:audio/flac:*`).
pub fn codec_from_mime(mime: &str) -> Option<&'static str> {
    let m = mime.to_ascii_lowercase();
    let m = m.split(';').next().unwrap_or("").trim();
    Some(match m {
        "audio/flac" | "audio/x-flac" => "FLAC",
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => "WAV",
        "audio/aiff" | "audio/x-aiff" => "AIFF",
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" => "MP3",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "AAC/ALAC",
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "AAC",
        "audio/ogg" | "audio/x-ogg" | "audio/vorbis" => "Vorbis",
        "audio/l16" => "PCM",
        _ => return None,
    })
}

#[derive(Default)]
struct Res {
    uri: String,
    duration: Option<String>,
    protocol: Option<String>,
    rate: Option<u32>,
    bits: Option<u8>,
}

fn res_attrs(e: &BytesStart<'_>) -> Res {
    let mut r = Res::default();
    for a in e.attributes().flatten() {
        let v = xml::attr_value(&a);
        match a.key.local_name().as_ref() {
            "duration" => r.duration = Some(v),
            "protocolInfo" => r.protocol = Some(v),
            "sampleFrequency" => r.rate = v.trim().parse().ok(),
            "bitsPerSample" => r.bits = v.trim().parse().ok(),
            _ => {}
        }
    }
    r
}

/// Pull the useful fields of the first item of a DIDL-Lite blob.
pub fn parse_didl(uri: &str, meta: &str) -> Option<TrackInfo> {
    if meta.trim().is_empty() {
        return None;
    }
    let mut reader = Reader::from_str(meta);
    let mut info = TrackInfo {
        uri: uri.into(),
        ..Default::default()
    };
    let mut res: Vec<Res> = Vec::new();
    let mut class = String::new();
    let mut artist_any: Option<String> = None;
    let mut cur: Option<(String, Option<String>)> = None; // (element, role)
    let mut text = String::new();
    let mut items = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                if name == "item" || name == "container" {
                    items += 1;
                    if items > 1 {
                        break;
                    }
                    continue;
                }
                if name == "res" {
                    res.push(res_attrs(&e));
                }
                let role = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.local_name().as_ref() == "role")
                    .map(|a| xml::attr_value(&a));
                cur = Some((name, role));
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == "res" {
                    res.push(res_attrs(&e));
                }
            }
            Ok(Event::Text(t)) => {
                if cur.is_some() {
                    text.push_str(&t.into_inner());
                }
            }
            Ok(Event::CData(t)) => {
                if cur.is_some() {
                    text.push_str(&t.into_inner());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if cur.is_some() {
                    text.push_str(&xml::resolve_ref(&r));
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_string();
                if let Some((el, role)) = cur.take()
                    && el == name
                {
                    let val = text.trim().to_string();
                    text.clear();
                    if !val.is_empty() {
                        apply(
                            &mut info,
                            &mut artist_any,
                            &mut res,
                            &mut class,
                            &el,
                            role,
                            val,
                        );
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if info.artist.is_none() {
        info.artist = artist_any;
    }
    // The resource that matches the URI we play, else the first one.
    let pick = res
        .iter()
        .position(|r| r.uri == uri)
        .or((!res.is_empty()).then_some(0));
    if let Some(r) = pick.map(|i| &res[i]) {
        if let Some(d) = r.duration.as_deref().and_then(parse_duration) {
            info.duration_ms = d;
        }
        info.sample_rate = r.rate.filter(|r| *r > 0);
        info.bits = r.bits.filter(|b| *b > 0);
        if let Some(p) = &r.protocol {
            let mime = p.split(':').nth(2).unwrap_or("");
            info.codec = codec_from_mime(mime).map(str::to_string);
        }
    }
    if class.contains("audioBroadcast") {
        info.live = true;
    }
    if info.title.is_empty() && info.artist.is_none() && info.album.is_none() && pick.is_none() {
        return None;
    }
    Some(info)
}

fn apply(
    info: &mut TrackInfo,
    artist_any: &mut Option<String>,
    res: &mut [Res],
    class: &mut String,
    el: &str,
    role: Option<String>,
    val: String,
) {
    match el {
        "title" if info.title.is_empty() => info.title = val,
        "artist" => match role.as_deref() {
            None | Some("") | Some("Performer") | Some("performer") => {
                if info.artist.is_none() {
                    info.artist = Some(val)
                }
            }
            Some(r) if r.eq_ignore_ascii_case("AlbumArtist") => {
                if info.album_artist.is_none() {
                    info.album_artist = Some(val)
                }
            }
            _ => {
                if artist_any.is_none() {
                    *artist_any = Some(val)
                }
            }
        },
        "creator" => {
            if artist_any.is_none() {
                *artist_any = Some(val)
            }
        }
        "album" if info.album.is_none() => info.album = Some(val),
        "albumArtURI" if info.cover.is_none() => info.cover = Some(val),
        "genre" if info.genre.is_none() => info.genre = Some(val),
        "originalTrackNumber" => info.track_no = val.parse().ok(),
        "date" => info.year = val.get(..4).and_then(|y| y.parse().ok()),
        "class" => *class = val,
        "res" => {
            if let Some(r) = res.last_mut() {
                r.uri = val;
            }
        }
        _ => {}
    }
}

/// One `<item>` element describing a track.
pub struct ItemXml<'a> {
    pub id: &'a str,
    pub parent: &'a str,
    pub info: &'a TrackInfo,
    /// URL a control point can fetch (never a `file://` URI).
    pub res_url: &'a str,
    pub protocol_info: &'a str,
    pub size: Option<u64>,
    pub channels: Option<u8>,
    /// Bytes per second (DLNA `bitrate`).
    pub bitrate: Option<u32>,
    pub art_url: Option<&'a str>,
}

impl ItemXml<'_> {
    pub fn render(&self) -> String {
        let t = self.info;
        let e = xml::escape;
        let mut s = format!(
            "<item id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title>",
            e(self.id),
            e(self.parent),
            e(&t.title)
        );
        if let Some(a) = &t.artist {
            s.push_str(&format!(
                "<dc:creator>{0}</dc:creator><upnp:artist>{0}</upnp:artist>",
                e(a)
            ));
        }
        if let Some(a) = &t.album_artist {
            s.push_str(&format!(
                "<upnp:artist role=\"AlbumArtist\">{}</upnp:artist>",
                e(a)
            ));
        }
        if let Some(a) = &t.album {
            s.push_str(&format!("<upnp:album>{}</upnp:album>", e(a)));
        }
        if let Some(g) = &t.genre {
            s.push_str(&format!("<upnp:genre>{}</upnp:genre>", e(g)));
        }
        if let Some(y) = t.year {
            s.push_str(&format!("<dc:date>{y:04}-01-01</dc:date>"));
        }
        if let Some(n) = t.track_no {
            s.push_str(&format!(
                "<upnp:originalTrackNumber>{n}</upnp:originalTrackNumber>"
            ));
        }
        if let Some(a) = self.art_url {
            s.push_str(&format!(
                "<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">{}</upnp:albumArtURI>",
                e(a)
            ));
        }
        let class = if t.live {
            "object.item.audioItem.audioBroadcast"
        } else {
            "object.item.audioItem.musicTrack"
        };
        s.push_str(&format!("<upnp:class>{class}</upnp:class>"));
        s.push_str(&format!("<res protocolInfo=\"{}\"", e(self.protocol_info)));
        if t.duration_ms > 0 {
            s.push_str(&format!(" duration=\"{}\"", fmt_duration(t.duration_ms)));
        }
        if let Some(sz) = self.size {
            s.push_str(&format!(" size=\"{sz}\""));
        }
        if let Some(b) = self.bitrate {
            s.push_str(&format!(" bitrate=\"{b}\""));
        }
        if let Some(b) = t.bits {
            s.push_str(&format!(" bitsPerSample=\"{b}\""));
        }
        if let Some(r) = t.sample_rate {
            s.push_str(&format!(" sampleFrequency=\"{r}\""));
        }
        if let Some(c) = self.channels {
            s.push_str(&format!(" nrAudioChannels=\"{c}\""));
        }
        s.push_str(&format!(">{}</res></item>", e(self.res_url)));
        s
    }

    pub fn document(&self) -> String {
        format!("{DIDL_OPEN}{}{DIDL_CLOSE}", self.render())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0:03:25.500"), Some(205_500));
        assert_eq!(parse_duration("1:00:00"), Some(3_600_000));
        assert_eq!(parse_duration("0:00:01.5"), Some(1_500));
        assert_eq!(parse_duration("0:00:01.1/2"), Some(1_500));
        assert_eq!(parse_duration("0:61:00"), None);
        assert_eq!(parse_duration("junk"), None);
        assert_eq!(fmt_duration(205_500), "0:03:25.500");
    }

    #[test]
    fn full_item() {
        let meta = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>Tom &amp; Jerry</dc:title><dc:creator>Composer</dc:creator><upnp:artist role="AlbumArtist">Band</upnp:artist><upnp:artist>Singer</upnp:artist><upnp:album>LP</upnp:album><upnp:genre>Jazz</upnp:genre><upnp:originalTrackNumber>7</upnp:originalTrackNumber><dc:date>1999-05-01</dc:date><upnp:albumArtURI>http://h/art.jpg</upnp:albumArtURI><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:audio/x-flac:*" duration="0:04:05.250" sampleFrequency="96000" bitsPerSample="24" nrAudioChannels="2">http://h/a.flac</res></item></DIDL-Lite>"#;
        let t = parse_didl("http://h/a.flac", meta).unwrap();
        assert_eq!(t.title, "Tom & Jerry");
        assert_eq!(t.artist.as_deref(), Some("Singer"));
        assert_eq!(t.album_artist.as_deref(), Some("Band"));
        assert_eq!(t.album.as_deref(), Some("LP"));
        assert_eq!(t.genre.as_deref(), Some("Jazz"));
        assert_eq!(t.track_no, Some(7));
        assert_eq!(t.year, Some(1999));
        assert_eq!(t.cover.as_deref(), Some("http://h/art.jpg"));
        assert_eq!(t.duration_ms, 245_250);
        assert_eq!(t.sample_rate, Some(96_000));
        assert_eq!(t.bits, Some(24));
        assert_eq!(t.codec.as_deref(), Some("FLAC"));
        assert!(!t.live);
    }

    #[test]
    fn creator_fallback_and_radio() {
        let meta = r#"<DIDL-Lite><item><dc:title>Radio</dc:title><dc:creator>Someone</dc:creator><upnp:class>object.item.audioItem.audioBroadcast</upnp:class></item></DIDL-Lite>"#;
        let t = parse_didl("http://r/stream", meta).unwrap();
        assert_eq!(t.artist.as_deref(), Some("Someone"));
        assert!(t.live);
        assert!(parse_didl("x", "").is_none());
    }

    #[test]
    fn render_parses_back() {
        let info = TrackInfo {
            title: "A <b>".into(),
            artist: Some("X & Y".into()),
            duration_ms: 61_000,
            sample_rate: Some(44_100),
            bits: Some(16),
            track_no: Some(2),
            ..Default::default()
        };
        let doc = ItemXml {
            id: "5",
            parent: "0",
            info: &info,
            res_url: "http://h/m?a=1&b=2",
            protocol_info: "http-get:*:audio/flac:*",
            size: Some(10),
            channels: Some(2),
            bitrate: None,
            art_url: None,
        }
        .document();
        let back = parse_didl("http://h/m?a=1&b=2", &doc).unwrap();
        assert_eq!(back.title, "A <b>");
        assert_eq!(back.artist.as_deref(), Some("X & Y"));
        assert_eq!(back.duration_ms, 61_000);
        assert_eq!(back.track_no, Some(2));
        assert_eq!(back.codec.as_deref(), Some("FLAC"));
    }
}
