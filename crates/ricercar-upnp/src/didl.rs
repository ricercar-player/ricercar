use quick_xml::Reader;
use quick_xml::events::Event;

use ricercar_core::TrackInfo;

/// Pull dc:title / upnp:artist / upnp:album / albumArtURI out of a DIDL-Lite
/// metadata blob.
pub fn parse_didl(uri: &str, meta: &str) -> Option<TrackInfo> {
    if meta.trim().is_empty() {
        return None;
    }
    let mut reader = Reader::from_str(meta);
    let mut title = None;
    let mut artist = None;
    let mut album = None;
    let mut art = None;
    let mut want: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_string();
                want = Some(name);
            }
            Ok(Event::Text(t)) => {
                if let Some(w) = want.take() {
                    let val = t.into_inner().trim().to_string();
                    if val.is_empty() {
                        continue;
                    }
                    match w.as_str() {
                        "title" => title = Some(val),
                        "creator" | "artist" => {
                            if artist.is_none() {
                                artist = Some(val)
                            }
                        }
                        "album" => album = Some(val),
                        "albumArtURI" => art = Some(val),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => want = None,
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    if title.is_none() && artist.is_none() && album.is_none() {
        return None;
    }
    Some(TrackInfo {
        uri: uri.into(),
        title: title.unwrap_or_default(),
        artist,
        album,
        duration_ms: 0,
        cover: art,
    })
}
