//! Lyrics: LRC parsing, current-line lookup, LRCLIB fetching and caching.

mod cache;
pub mod lrclib;

pub use cache::{CacheLookup, LyricsCache, NEGATIVE_TTL_SECS};

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsSource {
    Lrclib,
    /// Tags embedded in the audio file (for callers that read them).
    Embedded,
    /// A `.lrc` file next to the audio file.
    Sidecar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    /// Time-synced lines, sorted by time.
    pub synced: Option<Vec<LyricLine>>,
    pub plain: Option<String>,
    pub source: LyricsSource,
    /// The track is known to have no vocals (both texts are then `None`).
    #[serde(default)]
    pub instrumental: bool,
}

/// Parses LRC text into lines sorted by time.
///
/// Supports `[mm:ss]`, `[mm:ss.x]`, `[mm:ss.xx]`, `[mm:ss.xxx]` (and the
/// legacy `[mm:ss:xx]`), several timestamps on one line, and a global
/// `[offset:±ms]` (positive = lyrics shown earlier, per the LRC convention).
/// Metadata tags (`[ar:]`, `[ti:]`, `[al:]`, `[by:]`, `[length:]`, ...) and
/// lines without timestamps are dropped; enhanced-LRC word stamps
/// (`<mm:ss.xx>`) are stripped from the text. Empty timed lines are kept, as
/// they mark instrumental breaks where the display should clear.
pub fn parse_lrc(input: &str) -> Vec<LyricLine> {
    let mut offset_ms: i64 = 0;
    let mut timed: Vec<(u64, String)> = Vec::new();
    for raw in input.lines() {
        let mut rest = raw.trim_start_matches('\u{feff}').trim();
        let mut stamps = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            let tag = &after[..end];
            if let Some(t) = parse_timestamp(tag) {
                stamps.push(t);
            } else if !stamps.is_empty() {
                // e.g. "[00:10.00][Chorus]": the bracket is part of the text.
                break;
            } else if let Some(value) = tag_value(tag, "offset")
                && let Ok(o) = value.parse::<i64>()
            {
                offset_ms = o;
            }
            rest = after[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let text = strip_word_stamps(rest).trim().to_owned();
        timed.extend(stamps.into_iter().map(|t| (t, text.clone())));
    }
    let mut lines: Vec<LyricLine> = timed
        .into_iter()
        .map(|(t, text)| LyricLine {
            time_ms: (t as i64 - offset_ms).max(0) as u64,
            text,
        })
        .collect();
    lines.sort_by_key(|l| l.time_ms);
    lines
}

/// Index of the line being sung at `pos_ms` (last line starting at or
/// before it), or `None` before the first line. `lines` must be sorted.
pub fn line_at(lines: &[LyricLine], pos_ms: u64) -> Option<usize> {
    lines
        .partition_point(|l| l.time_ms <= pos_ms)
        .checked_sub(1)
}

/// Looks lyrics up in `cache`, falling back to LRCLIB and caching the result
/// (including "not found"). Network errors are returned and not cached.
pub fn fetch_cached(
    cache: &LyricsCache,
    artist: &str,
    title: &str,
    album: Option<&str>,
    duration_s: Option<u32>,
) -> Result<Option<Lyrics>> {
    let key = LyricsCache::key(artist, title, album, duration_s);
    match cache.lookup(&key) {
        CacheLookup::Hit(l) => return Ok(Some(l)),
        CacheLookup::KnownMissing => return Ok(None),
        CacheLookup::Miss => {}
    }
    let found = lrclib::get(artist, title, album, duration_s)?;
    if let Err(e) = cache.store(&key, found.as_ref()) {
        tracing::warn!(error = %e, "failed to cache lyrics");
    }
    Ok(found)
}

fn parse_timestamp(tag: &str) -> Option<u64> {
    let mut parts = tag.trim().split(':');
    let min = parts.next()?;
    let sec_part = parts.next()?;
    let legacy_frac = parts.next();
    if parts.next().is_some() || !all_digits(min) {
        return None;
    }
    let (sec, frac) = match (sec_part.split_once('.'), legacy_frac) {
        (Some((s, f)), None) => (s, Some(f)),
        (None, frac) => (sec_part, frac),
        (Some(_), Some(_)) => return None,
    };
    if !all_digits(sec) || sec.len() > 2 {
        return None;
    }
    let sec: u64 = sec.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    let frac_ms = match frac {
        None => 0,
        Some(f) if all_digits(f) => {
            // Scale to milliseconds: "5" -> 500, "05" -> 50, "005" -> 5.
            let digits = &f[..f.len().min(3)];
            digits.parse::<u64>().ok()? * 10u64.pow(3 - digits.len() as u32)
        }
        Some(_) => return None,
    };
    let min: u64 = min.parse().ok()?;
    Some(min * 60_000 + sec * 1000 + frac_ms)
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn tag_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let (k, v) = tag.split_once(':')?;
    k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
}

fn strip_word_stamps(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 1..start + len];
        if parse_timestamp(inner).is_none() {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(lines: &[LyricLine]) -> Vec<u64> {
        lines.iter().map(|l| l.time_ms).collect()
    }

    fn texts(lines: &[LyricLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn timestamp_formats() {
        assert_eq!(parse_timestamp("00:00"), Some(0));
        assert_eq!(parse_timestamp("01:02"), Some(62_000));
        assert_eq!(parse_timestamp("01:02.5"), Some(62_500));
        assert_eq!(parse_timestamp("01:02.34"), Some(62_340));
        assert_eq!(parse_timestamp("01:02.345"), Some(62_345));
        assert_eq!(parse_timestamp("01:02.3456"), Some(62_345));
        assert_eq!(parse_timestamp("01:02:34"), Some(62_340));
        assert_eq!(parse_timestamp("123:00.00"), Some(7_380_000));
        assert_eq!(parse_timestamp("1:5.1"), Some(65_100));
    }

    #[test]
    fn invalid_timestamps() {
        for t in [
            "ar:Artist",
            "",
            "00",
            "00:60",
            "aa:10",
            "00:1a",
            "00:10.x",
            "00:10.",
            "-1:00",
            "00:100",
            "00:01.00:00",
            "offset:+100",
        ] {
            assert_eq!(parse_timestamp(t), None, "{t:?}");
        }
    }

    #[test]
    fn basic_file_with_metadata() {
        let lrc = "\u{feff}[ar:Björk]\r\n[ti:Jóga]\r\n[al:Homogenic]\r\n[by:someone]\r\n[length: 05:05]\r\n\
                   [00:12.00]All these accidents\r\n[00:17.20]That happen\r\n";
        let lines = parse_lrc(lrc);
        assert_eq!(times(&lines), [12_000, 17_200]);
        assert_eq!(texts(&lines), ["All these accidents", "That happen"]);
    }

    #[test]
    fn multiple_timestamps_per_line_are_sorted() {
        let lrc = "[00:10.00]Verse\n[00:05.00][00:20.00]Chorus\n[00:15.00]Bridge";
        let lines = parse_lrc(lrc);
        assert_eq!(times(&lines), [5_000, 10_000, 15_000, 20_000]);
        assert_eq!(texts(&lines), ["Chorus", "Verse", "Bridge", "Chorus"]);
    }

    #[test]
    fn positive_offset_shows_lyrics_earlier() {
        let lines = parse_lrc("[offset:+500]\n[00:10.00]a\n[00:00.20]b");
        assert_eq!(times(&lines), [0, 9_500]);
    }

    #[test]
    fn negative_offset_delays_lyrics() {
        let lines = parse_lrc("[00:10.00]a\n[offset: -250]");
        assert_eq!(times(&lines), [10_250]);
    }

    #[test]
    fn bad_offset_is_ignored() {
        let lines = parse_lrc("[offset:abc]\n[00:01.00]a");
        assert_eq!(times(&lines), [1_000]);
    }

    #[test]
    fn empty_lines_are_kept_and_untimed_lines_dropped() {
        let lines = parse_lrc("plain text\n[00:01.00]\n[00:02.00]  hello  \n\n");
        assert_eq!(texts(&lines), ["", "hello"]);
    }

    #[test]
    fn bracket_text_after_timestamp_is_text() {
        let lines = parse_lrc("[00:01.00][Chorus] la la");
        assert_eq!(texts(&lines), ["[Chorus] la la"]);
    }

    #[test]
    fn unterminated_bracket() {
        let lines = parse_lrc("[00:01.00]a [b\n[00:02.00\n[00:03");
        assert_eq!(texts(&lines), ["a [b"]);
    }

    #[test]
    fn enhanced_word_stamps_are_stripped() {
        let lines = parse_lrc("[00:01.00]<00:01.00>Hello <00:01.50>world <b>");
        assert_eq!(texts(&lines), ["Hello world <b>"]);
    }

    #[test]
    fn equal_times_keep_file_order() {
        let lines = parse_lrc("[00:01.00]first\n[00:01.00]second");
        assert_eq!(texts(&lines), ["first", "second"]);
    }

    #[test]
    fn empty_input() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("[ar:x]\n[ti:y]").is_empty());
    }

    #[test]
    fn fixture_file() {
        let lrc = include_str!("../../tests/fixtures/sample.lrc");
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[4].text, "");
        assert_eq!(lines[7].text, "It's not my fault");
        assert!(lines.windows(2).all(|w| w[0].time_ms <= w[1].time_ms));
        assert_eq!(lines[0].text, "I feel your breath upon my neck");
        assert_eq!(lines[0].time_ms, 17_120 - 120);
    }

    #[test]
    fn line_at_binary_search() {
        let lines = parse_lrc("[00:01.00]a\n[00:02.00]b\n[00:02.00]c\n[00:05.00]d");
        assert_eq!(line_at(&lines, 0), None);
        assert_eq!(line_at(&lines, 999), None);
        assert_eq!(line_at(&lines, 1_000), Some(0));
        assert_eq!(line_at(&lines, 1_999), Some(0));
        assert_eq!(line_at(&lines, 2_000), Some(2));
        assert_eq!(line_at(&lines, 4_999), Some(2));
        assert_eq!(line_at(&lines, 60_000), Some(3));
        assert_eq!(line_at(&[], 1_000), None);
    }

    #[test]
    fn lyrics_serde_roundtrip() {
        let l = Lyrics {
            synced: Some(parse_lrc("[00:01.00]a")),
            plain: Some("a".into()),
            source: LyricsSource::Lrclib,
            instrumental: false,
        };
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains("\"lrclib\""));
        assert_eq!(serde_json::from_str::<Lyrics>(&json).unwrap(), l);
    }
}
