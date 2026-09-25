//! LRCLIB (<https://lrclib.net/docs>): free, open lyrics database, no key.

use serde::Deserialize;

use super::{Lyrics, LyricsSource, parse_lrc};
use crate::Result;
use crate::http::{self, agent};

pub const API_BASE: &str = "https://lrclib.net/api";
/// LRCLIB's own matching tolerance for `duration`.
const DURATION_TOLERANCE_S: f64 = 2.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

/// Fetches lyrics for a track: exact `/api/get` first, then `/api/search`
/// picking the best candidate within ±2 s of `duration_s`.
pub fn get(
    artist: &str,
    title: &str,
    album: Option<&str>,
    duration_s: Option<u32>,
) -> Result<Option<Lyrics>> {
    let mut req = agent()
        .get(&format!("{API_BASE}/get"))
        .query("artist_name", artist)
        .query("track_name", title);
    if let Some(album) = album.filter(|a| !a.trim().is_empty()) {
        req = req.query("album_name", album);
    }
    if let Some(d) = duration_s {
        req = req.query("duration", &d.to_string());
    }
    match http::get_json::<Record>(req) {
        Ok(record) => {
            if let Some(lyrics) = to_lyrics(record) {
                return Ok(Some(lyrics));
            }
        }
        Err(e) if http::is_not_found(&e) => {}
        Err(e) => return Err(e),
    }

    let req = agent()
        .get(&format!("{API_BASE}/search"))
        .query("artist_name", artist)
        .query("track_name", title);
    let records: Vec<Record> = http::get_json(req)?;
    Ok(best_match(records, duration_s).and_then(to_lyrics))
}

/// Converts a record, or `None` if it carries neither lyrics nor the
/// instrumental flag.
pub(crate) fn to_lyrics(r: Record) -> Option<Lyrics> {
    let clean = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let synced = clean(r.synced_lyrics)
        .map(|s| parse_lrc(&s))
        .filter(|l| !l.is_empty());
    let plain = clean(r.plain_lyrics);
    if synced.is_none() && plain.is_none() && !r.instrumental {
        return None;
    }
    Some(Lyrics {
        synced,
        plain,
        source: LyricsSource::Lrclib,
        instrumental: r.instrumental,
    })
}

/// Best search candidate: within the duration tolerance (when known),
/// preferring synced lyrics, then the closest duration, then plain lyrics.
pub(crate) fn best_match(records: Vec<Record>, duration_s: Option<u32>) -> Option<Record> {
    let diff = |r: &Record| match (duration_s, r.duration) {
        (Some(want), Some(have)) => Some((have - f64::from(want)).abs()),
        (Some(_), None) => None,
        (None, _) => Some(0.0),
    };
    let has = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.trim().is_empty());
    records
        .into_iter()
        .filter(|r| has(&r.synced_lyrics) || has(&r.plain_lyrics) || r.instrumental)
        .filter_map(|r| {
            let d = diff(&r)?;
            (d <= DURATION_TOLERANCE_S).then_some((r, d))
        })
        .min_by(|(a, da), (b, db)| {
            (!has(&a.synced_lyrics))
                .cmp(&!has(&b.synced_lyrics))
                .then(da.total_cmp(db))
                .then((!has(&a.plain_lyrics)).cmp(&!has(&b.plain_lyrics)))
        })
        .map(|(r, _)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_fixture() -> Vec<Record> {
        serde_json::from_str(include_str!("../../tests/fixtures/lrclib_search.json")).unwrap()
    }

    #[test]
    fn get_fixture_parses() {
        let r: Record =
            serde_json::from_str(include_str!("../../tests/fixtures/lrclib_get.json")).unwrap();
        let l = to_lyrics(r).unwrap();
        assert_eq!(l.source, LyricsSource::Lrclib);
        let synced = l.synced.unwrap();
        assert_eq!(synced[0].time_ms, 17_120);
        assert_eq!(synced[0].text, "I feel your breath upon my neck");
        assert!(l.plain.unwrap().starts_with("I feel your breath"));
        assert!(!l.instrumental);
    }

    #[test]
    fn instrumental_record() {
        let r: Record = serde_json::from_str(
            r#"{"id":1,"duration":120.0,"instrumental":true,"plainLyrics":null,"syncedLyrics":null}"#,
        )
        .unwrap();
        let l = to_lyrics(r).unwrap();
        assert!(l.instrumental);
        assert!(l.synced.is_none() && l.plain.is_none());
    }

    #[test]
    fn record_without_lyrics_is_none() {
        let r: Record = serde_json::from_str(
            r#"{"duration":120.0,"instrumental":false,"plainLyrics":"  ","syncedLyrics":""}"#,
        )
        .unwrap();
        assert!(to_lyrics(r).is_none());
    }

    #[test]
    fn best_match_prefers_synced_within_tolerance() {
        // Fixture: 282 s synced, 233 s plain-only, 234.5 s synced, 36 s synced,
        // 233 s without any lyrics, null duration synced.
        let best = best_match(search_fixture(), Some(233)).unwrap();
        assert_eq!(best.duration, Some(234.5));
        assert!(best.synced_lyrics.is_some());
    }

    #[test]
    fn best_match_falls_back_to_plain() {
        let mut records = search_fixture();
        records.retain(|r| r.duration != Some(234.5));
        let best = best_match(records, Some(233)).unwrap();
        assert_eq!(best.duration, Some(233.0));
        assert!(best.synced_lyrics.is_none());
    }

    #[test]
    fn best_match_nothing_close_enough() {
        assert!(best_match(search_fixture(), Some(100)).is_none());
        assert!(best_match(Vec::new(), None).is_none());
    }

    #[test]
    fn best_match_without_duration_takes_first_synced() {
        let best = best_match(search_fixture(), None).unwrap();
        assert_eq!(best.duration, Some(282.0));
    }

    // Hits lrclib.net.
    #[test]
    #[ignore]
    fn live_get() {
        let l = get("Borislav Slavov", "I Want to Live", None, Some(233)).unwrap();
        assert!(l.is_some());
    }
}
