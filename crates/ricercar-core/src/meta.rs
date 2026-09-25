use std::path::{Path, PathBuf};

use lofty::file::{FileType, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

/// Tags + technical metadata for one track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub channels: Option<u8>,
    /// kbit/s
    pub bitrate: Option<u32>,
    pub codec: Option<String>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub mb_album_id: Option<String>,
    pub mb_track_id: Option<String>,
}

fn parse_u32(s: Option<&str>) -> Option<u32> {
    s.and_then(|v| v.split('/').next().unwrap_or(v).trim().parse::<u32>().ok())
}

fn parse_year(s: Option<&str>) -> Option<i32> {
    s.and_then(|v| {
        v.trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    })
    .filter(|y| (1000..3000).contains(y))
}

/// "-6.54 dB" / "+1.2" → -6.54 / 1.2
fn parse_gain(s: Option<&str>) -> Option<f32> {
    let s = s?.trim();
    let num = s
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim();
    num.parse::<f32>().ok().filter(|g| g.is_finite())
}

fn codec_name(ft: &FileType) -> &'static str {
    match ft {
        FileType::Flac => "FLAC",
        FileType::Mpeg => "MP3",
        FileType::Wav => "WAV",
        FileType::Aiff => "AIFF",
        FileType::Vorbis => "Vorbis",
        FileType::Opus => "Opus",
        FileType::Mp4 => "AAC/ALAC",
        FileType::Ape => "APE",
        FileType::WavPack => "WavPack",
        FileType::Speex => "Speex",
        _ => "?",
    }
}

fn text(tag: Option<&Tag>, key: ItemKey) -> Option<String> {
    tag.and_then(|t| t.get_string(key))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn read_tags(path: &Path) -> TagInfo {
    let fallback_title = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tf = match Probe::open(path).and_then(|p| p.read()) {
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
    let year_src = [
        ItemKey::RecordingDate,
        ItemKey::Year,
        ItemKey::ReleaseDate,
        ItemKey::OriginalReleaseDate,
    ]
    .into_iter()
    .find_map(|k| parse_year(tag.and_then(|t| t.get_string(k))));
    let mut codec = codec_name(&tf.file_type()).to_string();
    if matches!(tf.file_type(), FileType::Mp4) {
        // MP4 can hold AAC or ALAC: a bit depth means lossless ALAC.
        codec = if props.bit_depth().is_some() {
            "ALAC".into()
        } else {
            "AAC".into()
        };
    }
    let mut info = TagInfo {
        title: text(tag, ItemKey::TrackTitle),
        artist: text(tag, ItemKey::TrackArtist),
        album_artist: text(tag, ItemKey::AlbumArtist),
        album: text(tag, ItemKey::AlbumTitle),
        track: parse_u32(tag.and_then(|t| t.get_string(ItemKey::TrackNumber))),
        disc: parse_u32(tag.and_then(|t| t.get_string(ItemKey::DiscNumber))),
        year: year_src,
        genre: text(tag, ItemKey::Genre),
        composer: text(tag, ItemKey::Composer),
        duration_ms: props.duration().as_millis() as u64,
        sample_rate: props.sample_rate(),
        bits: props.bit_depth(),
        channels: props.channels(),
        bitrate: props.audio_bitrate().or(props.overall_bitrate()),
        codec: Some(codec),
        rg_track_gain: parse_gain(tag.and_then(|t| t.get_string(ItemKey::ReplayGainTrackGain))),
        rg_track_peak: parse_gain(tag.and_then(|t| t.get_string(ItemKey::ReplayGainTrackPeak))),
        rg_album_gain: parse_gain(tag.and_then(|t| t.get_string(ItemKey::ReplayGainAlbumGain))),
        rg_album_peak: parse_gain(tag.and_then(|t| t.get_string(ItemKey::ReplayGainAlbumPeak))),
        mb_album_id: text(tag, ItemKey::MusicBrainzReleaseId),
        mb_track_id: text(tag, ItemKey::MusicBrainzRecordingId),
    };
    if info.title.is_none() {
        info.title = Some(fallback_title);
    }
    info
}

/// Embedded unsynced lyrics or a sidecar `.lrc` next to the file.
pub fn local_lyrics(path: &Path) -> Option<String> {
    let lrc = path.with_extension("lrc");
    if let Ok(s) = std::fs::read_to_string(&lrc)
        && !s.trim().is_empty()
    {
        return Some(s);
    }
    let tf = Probe::open(path).ok()?.read().ok()?;
    let tag = tf.primary_tag().or_else(|| tf.first_tag());
    text(tag, ItemKey::Lyrics).or_else(|| text(tag, ItemKey::UnsyncLyrics))
}

/// First embedded picture: (bytes, mime).
pub fn embedded_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let tf = Probe::open(path).ok()?.read().ok()?;
    let tag = tf.primary_tag().or_else(|| tf.first_tag())?;
    let pics = tag.pictures();
    let front = pics
        .iter()
        .find(|p| matches!(p.pic_type(), PictureType::CoverFront))
        .or_else(|| {
            pics.iter()
                .find(|p| !matches!(p.pic_type(), PictureType::Other))
        })
        .or_else(|| pics.first())?;
    let mime = front
        .mime_type()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "image/jpeg".into());
    Some((front.data().to_vec(), mime))
}

const COVER_NAMES: &[&str] = &[
    "cover",
    "folder",
    "front",
    "album",
    "albumart",
    "albumartsmall",
];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// `cover.jpg` & co. in the track's directory (or its parent for `CD1/`
/// style multi-disc layouts).
pub fn folder_cover(track: &Path) -> Option<PathBuf> {
    let dir = track.parent()?;
    let scan = |d: &Path| -> Option<PathBuf> {
        let mut images: Vec<PathBuf> = std::fs::read_dir(d)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            })
            .collect();
        images.sort();
        let named = images.iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| COVER_NAMES.contains(&s.to_ascii_lowercase().as_str()))
        });
        named.cloned().or_else(|| images.into_iter().next())
    };
    scan(dir).or_else(|| {
        let name = dir.file_name()?.to_str()?.to_ascii_lowercase();
        let disc_dir = ["cd", "disc", "disk"].iter().any(|p| name.starts_with(p));
        if disc_dir { scan(dir.parent()?) } else { None }
    })
}

/// Embedded art first, then folder art: (bytes, mime).
pub fn cover_bytes(path: &Path) -> Option<(Vec<u8>, String)> {
    embedded_cover(path).or_else(|| {
        let p = folder_cover(path)?;
        let bytes = std::fs::read(&p).ok()?;
        let mime = match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "webp" => "image/webp",
            _ => "image/jpeg",
        };
        Some((bytes, mime.into()))
    })
}

pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://")
        .map(|p| PathBuf::from(percent_decode(p)))
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_with_special_chars() {
        let p = Path::new("/music/Björk/Début (1993)/01 #1 100%.flac");
        let uri = file_uri(p);
        assert!(!uri.contains(' ') && !uri.contains('#'));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn gains_and_years() {
        assert_eq!(parse_gain(Some("-6.54 dB")), Some(-6.54));
        assert_eq!(parse_gain(Some("+1.20 dB")), Some(1.2));
        assert_eq!(parse_gain(Some("0.988")), Some(0.988));
        assert_eq!(parse_gain(Some("n/a")), None);
        assert_eq!(parse_year(Some("1999-03-02")), Some(1999));
        assert_eq!(parse_year(Some("0")), None);
    }
}
