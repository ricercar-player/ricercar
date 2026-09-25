//! Missing album art: MusicBrainz release search, then the Cover Art Archive.
//!
//! MusicBrainz allows at most one request per second per client; every MB
//! call here goes through [`musicbrainz_limiter`], shared process-wide. The
//! Cover Art Archive has no such limit.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::http::{self, agent};
use crate::{OnlineError, Result};

pub const MUSICBRAINZ_API: &str = "https://musicbrainz.org/ws/2";
pub const COVER_ART_ARCHIVE: &str = "https://coverartarchive.org";
/// Search results scoring below this are too uncertain to use.
pub const MIN_SCORE: u32 = 90;
/// Largest image we accept; CAA "1200" thumbnails are far smaller.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRef {
    pub mbid: String,
    pub release_group_mbid: Option<String>,
    pub title: String,
    /// Credited artist as displayed (join phrases included).
    pub artist: String,
    pub date: Option<String>,
}

/// Cover Art Archive thumbnail sizes (longest edge, pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSize {
    S250,
    S500,
    S1200,
}

impl CoverSize {
    pub fn pixels(self) -> u32 {
        match self {
            CoverSize::S250 => 250,
            CoverSize::S500 => 500,
            CoverSize::S1200 => 1200,
        }
    }
}

/// Serializes callers so that consecutive `wait()` returns are at least
/// `interval` apart, across all threads.
#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    next_slot: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_slot: Mutex::new(None),
        }
    }

    /// Blocks until the caller may issue its request.
    pub fn wait(&self) {
        // The lock is held while sleeping on purpose: it queues the other
        // threads behind us so their slots are computed after ours.
        let mut next = self.next_slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *next {
            let now = Instant::now();
            if at > now {
                std::thread::sleep(at - now);
            }
        }
        *next = Some(Instant::now() + self.interval);
    }
}

/// The process-wide MusicBrainz limiter (1 request/second).
pub fn musicbrainz_limiter() -> &'static RateLimiter {
    static LIMITER: RateLimiter = RateLimiter::new(Duration::from_secs(1));
    &LIMITER
}

/// Escapes Lucene query syntax so user text is matched literally.
pub fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) fn release_query(artist: &str, album: &str) -> String {
    format!(
        "release:\"{}\" AND artist:\"{}\"",
        lucene_escape(album.trim()),
        lucene_escape(artist.trim())
    )
}

/// Finds the best-matching MusicBrainz release (score ≥ [`MIN_SCORE`]).
pub fn find_release(artist: &str, album: &str) -> Result<Option<ReleaseRef>> {
    if artist.trim().is_empty() || album.trim().is_empty() {
        return Ok(None);
    }
    let req = agent()
        .get(&format!("{MUSICBRAINZ_API}/release/"))
        .query("query", &release_query(artist, album))
        .query("fmt", "json")
        .query("limit", "10");
    musicbrainz_limiter().wait();
    let resp: SearchResponse = http::get_json(req)?;
    Ok(best_release(resp))
}

/// Downloads the front cover, trying the release then its release group.
/// `Ok(None)` when the archive has no front image.
pub fn front_cover(release: &ReleaseRef, size: CoverSize) -> Result<Option<Vec<u8>>> {
    let mut targets = vec![format!("release/{}", release.mbid)];
    if let Some(rg) = &release.release_group_mbid {
        targets.push(format!("release-group/{rg}"));
    }
    for target in targets {
        let url = format!("{COVER_ART_ARCHIVE}/{target}/front-{}", size.pixels());
        match http::send(agent().get(&url).call()) {
            Ok(resp) => {
                let bytes = http::read_bytes(resp, MAX_IMAGE_BYTES)?;
                if bytes.is_empty() {
                    return Err(OnlineError::Parse("empty cover image".into()));
                }
                return Ok(Some(bytes));
            }
            Err(e) if http::is_not_found(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchResponse {
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(Debug, Deserialize)]
struct MbRelease {
    id: String,
    #[serde(default)]
    score: u32,
    title: String,
    date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<MbCredit>,
    #[serde(rename = "release-group")]
    release_group: Option<MbId>,
}

#[derive(Debug, Deserialize)]
struct MbCredit {
    name: String,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Debug, Deserialize)]
struct MbId {
    id: String,
}

/// Highest score wins; ties keep MusicBrainz's order (its relevance ranking).
fn best_release(resp: SearchResponse) -> Option<ReleaseRef> {
    let best = resp
        .releases
        .into_iter()
        .filter(|r| r.score >= MIN_SCORE)
        .reduce(|best, r| if r.score > best.score { r } else { best })?;
    Some(ReleaseRef {
        artist: best
            .artist_credit
            .iter()
            .map(|c| format!("{}{}", c.name, c.joinphrase))
            .collect(),
        mbid: best.id,
        release_group_mbid: best.release_group.map(|g| g.id),
        title: best.title,
        date: best.date.filter(|d| !d.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn escaping() {
        assert_eq!(lucene_escape("AC/DC"), "AC\\/DC");
        assert_eq!(
            lucene_escape(r#"a+b-c&&d||e!(f){g}[h]^"i"~j*k?l:m\n"#),
            r#"a\+b\-c\&\&d\|\|e\!\(f\)\{g\}\[h\]\^\"i\"\~j\*k\?l\:m\\n"#
        );
        assert_eq!(lucene_escape("Sigur Rós"), "Sigur Rós");
    }

    #[test]
    fn query_building() {
        assert_eq!(
            release_query(" AC/DC ", "Highway to Hell"),
            r#"release:"Highway to Hell" AND artist:"AC\/DC""#
        );
    }

    #[test]
    fn picks_best_scored_release() {
        let resp: SearchResponse = serde_json::from_str(include_str!(
            "../tests/fixtures/musicbrainz_release_search.json"
        ))
        .unwrap();
        let r = best_release(resp).unwrap();
        assert_eq!(r.mbid, "4b3d18cc-8937-36f4-8de0-481088be58e6");
        assert_eq!(
            r.release_group_mbid.as_deref(),
            Some("b1392450-e666-3926-a536-22c65f834433")
        );
        assert_eq!(r.title, "OK Computer");
        assert_eq!(r.artist, "Radiohead");
        assert_eq!(r.date.as_deref(), Some("1997-06-17"));
    }

    #[test]
    fn joins_artist_credits_and_prefers_higher_score() {
        let resp: SearchResponse = serde_json::from_str(
            r#"{"releases":[
                {"id":"a","score":92,"title":"X","artist-credit":[{"name":"A"}]},
                {"id":"b","score":97,"title":"Y","date":"",
                 "artist-credit":[{"name":"Simon","joinphrase":" & "},{"name":"Garfunkel"}]}
            ]}"#,
        )
        .unwrap();
        let r = best_release(resp).unwrap();
        assert_eq!(r.mbid, "b");
        assert_eq!(r.artist, "Simon & Garfunkel");
        assert_eq!(r.date, None);
        assert_eq!(r.release_group_mbid, None);
    }

    #[test]
    fn low_scores_are_rejected() {
        let resp: SearchResponse = serde_json::from_str(
            r#"{"releases":[{"id":"a","score":89,"title":"X","artist-credit":[]}]}"#,
        )
        .unwrap();
        assert!(best_release(resp).is_none());
        let empty: SearchResponse = serde_json::from_str(r#"{"count":0}"#).unwrap();
        assert!(best_release(empty).is_none());
    }

    #[test]
    fn empty_input_skips_network() {
        assert_eq!(find_release("", "Album").unwrap(), None);
        assert_eq!(find_release("Artist", "  ").unwrap(), None);
    }

    #[test]
    fn cover_sizes() {
        assert_eq!(CoverSize::S250.pixels(), 250);
        assert_eq!(CoverSize::S500.pixels(), 500);
        assert_eq!(CoverSize::S1200.pixels(), 1200);
    }

    #[test]
    fn rate_limiter_spaces_requests_across_threads() {
        let interval = Duration::from_millis(40);
        let limiter = Arc::new(RateLimiter::new(interval));
        let start = Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let l = Arc::clone(&limiter);
                std::thread::spawn(move || l.wait())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // The first slot is free, the three others each wait one interval.
        assert!(start.elapsed() >= interval * 3);
    }

    #[test]
    fn rate_limiter_first_call_is_immediate() {
        let limiter = RateLimiter::new(Duration::from_secs(10));
        let start = Instant::now();
        limiter.wait();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    // Hits MusicBrainz and the Cover Art Archive.
    #[test]
    #[ignore]
    fn live_find_and_fetch() {
        let r = find_release("Radiohead", "OK Computer").unwrap().unwrap();
        let img = front_cover(&r, CoverSize::S250).unwrap().unwrap();
        assert!(img.starts_with(&[0xFF, 0xD8]) || img.starts_with(b"\x89PNG"));
    }
}
