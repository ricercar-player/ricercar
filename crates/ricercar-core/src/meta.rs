use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::ItemKey;

/// Tags + technical metadata for one track.
#[derive(Debug, Clone, Default)]
pub struct TagInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_ms: u64,
}

fn parse_u32(s: Option<&str>) -> Option<u32> {
    s.map(|v| v.split('/').next().unwrap_or(v).trim().parse::<u32>().ok())
        .flatten()
}

fn parse_year(s: Option<&str>) -> Option<i32> {
    s.and_then(|v| v.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
}

pub fn read_tags(path: &Path) -> TagInfo {
    let fallback_title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let read = Probe::open(path).and_then(|p| p.read());
    let tf = match read {
        Ok(tf) => tf,
        Err(_) => {
            return TagInfo {
                title: Some(fallback_title),
                ..Default::default()
            };
        }
    };
    let tag = tf.primary_tag().or_else(|| tf.first_tag());
    let props = tf.properties();
    let mut info = TagInfo {
        title: tag.and_then(|t| t.get_string(ItemKey::TrackTitle).map(|s| s.to_string())),
        artist: tag.and_then(|t| t.get_string(ItemKey::TrackArtist).map(|s| s.to_string())),
        album_artist: tag
            .and_then(|t| t.get_string(ItemKey::AlbumArtist).map(|s| s.to_string())),
        album: tag.and_then(|t| t.get_string(ItemKey::AlbumTitle).map(|s| s.to_string())),
        track: tag.and_then(|t| parse_u32(t.get_string(ItemKey::TrackNumber).as_deref())),
        disc: tag.and_then(|t| parse_u32(t.get_string(ItemKey::DiscNumber).as_deref())),
        year: tag.and_then(|t| parse_year(t.get_string(ItemKey::Year).as_deref())),
        genre: tag.and_then(|t| t.get_string(ItemKey::Genre).map(|s| s.to_string())),
        duration_ms: props.duration().as_millis() as u64,
    };
    if info.title.as_deref().unwrap_or("").is_empty() {
        info.title = Some(fallback_title);
    }
    info
}

/// First embedded picture: (bytes, mime).
pub fn cover_bytes(path: &Path) -> Option<(Vec<u8>, String)> {
    let tf = Probe::open(path).ok()?.read().ok()?;
    let tag = tf.primary_tag().or_else(|| tf.first_tag())?;
    let pics = tag.pictures();
    let front = pics
        .iter()
        .find(|p| matches!(p.pic_type(), PictureType::CoverFront))
        .or_else(|| {
            pics.iter().find(|p| {
                !matches!(p.pic_type(), PictureType::Other)
            })
        })
        .or_else(|| pics.first())?;
    let mime = front
        .mime_type()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "image/jpeg".into());
    Some((front.data().to_vec(), mime))
}

pub fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    uri.strip_prefix("file://")
        .map(|p| std::path::PathBuf::from(percent_decode(p)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
