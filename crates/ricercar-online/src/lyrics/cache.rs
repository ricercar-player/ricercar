//! On-disk lyrics cache: one JSON file per track, including negative results.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::Lyrics;
use crate::{OnlineError, Result, fsutil};

/// "Not found" entries are trusted for 7 days, then retried: LRCLIB is
/// crowd-sourced and gains lyrics over time.
pub const NEGATIVE_TTL_SECS: i64 = 7 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit(Lyrics),
    /// A recent lookup found nothing; don't ask again yet.
    KnownMissing,
    /// Never looked up, or the negative entry expired.
    Miss,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    fetched_at_unix: i64,
    lyrics: Option<Lyrics>,
}

#[derive(Debug, Clone)]
pub struct LyricsCache {
    dir: PathBuf,
}

impl LyricsCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stable key (md5 hex) over normalized `artist|title|album|duration`;
    /// case and whitespace differences map to the same entry.
    pub fn key(artist: &str, title: &str, album: Option<&str>, duration_s: Option<u32>) -> String {
        let norm = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        let raw = format!(
            "{}|{}|{}|{}",
            norm(artist),
            norm(title),
            album.map(norm).unwrap_or_default(),
            duration_s.map(|d| d.to_string()).unwrap_or_default()
        );
        fsutil::md5_hex(raw.as_bytes())
    }

    pub fn lookup(&self, key: &str) -> CacheLookup {
        self.lookup_at(key, now_unix())
    }

    /// Records a result; `None` caches a miss.
    pub fn store(&self, key: &str, lyrics: Option<&Lyrics>) -> Result<()> {
        self.store_at(key, lyrics, now_unix())
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn lookup_at(&self, key: &str, now: i64) -> CacheLookup {
        let Ok(bytes) = fs::read(self.path(key)) else {
            return CacheLookup::Miss;
        };
        match serde_json::from_slice::<Entry>(&bytes) {
            Ok(Entry {
                lyrics: Some(l), ..
            }) => CacheLookup::Hit(l),
            Ok(Entry {
                fetched_at_unix,
                lyrics: None,
            }) if now - fetched_at_unix < NEGATIVE_TTL_SECS => CacheLookup::KnownMissing,
            _ => CacheLookup::Miss,
        }
    }

    fn store_at(&self, key: &str, lyrics: Option<&Lyrics>, now: i64) -> Result<()> {
        let entry = Entry {
            fetched_at_unix: now,
            lyrics: lyrics.cloned(),
        };
        let json = serde_json::to_vec(&entry).map_err(|e| OnlineError::Parse(e.to_string()))?;
        fsutil::write_atomic(&self.path(key), &json)?;
        Ok(())
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::{LyricsSource, parse_lrc};

    fn lyrics() -> Lyrics {
        Lyrics {
            synced: Some(parse_lrc("[00:01.00]hi")),
            plain: Some("hi".into()),
            source: LyricsSource::Lrclib,
            instrumental: false,
        }
    }

    #[test]
    fn key_is_stable_and_normalized() {
        let a = LyricsCache::key("Björk", "Jóga", Some("Homogenic"), Some(305));
        let b = LyricsCache::key("  björk ", "JÓGA", Some("homogenic"), Some(305));
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        // Pinned so that a change of hashing scheme is a deliberate decision.
        assert_eq!(
            LyricsCache::key("a", "b", None, None),
            fsutil::md5_hex(b"a|b||")
        );
        assert_ne!(
            a,
            LyricsCache::key("Björk", "Jóga", Some("Homogenic"), Some(306))
        );
        assert_ne!(a, LyricsCache::key("Björk", "Jóga", None, Some(305)));
    }

    #[test]
    fn hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LyricsCache::new(dir.path().join("lyrics"));
        let key = LyricsCache::key("a", "b", None, None);
        assert_eq!(cache.lookup(&key), CacheLookup::Miss);
        cache.store(&key, Some(&lyrics())).unwrap();
        assert_eq!(cache.lookup(&key), CacheLookup::Hit(lyrics()));
        // Positive entries never expire.
        assert_eq!(
            cache.lookup_at(&key, now_unix() + 10 * NEGATIVE_TTL_SECS),
            CacheLookup::Hit(lyrics())
        );
    }

    #[test]
    fn negative_entries_expire_after_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LyricsCache::new(dir.path());
        let key = LyricsCache::key("a", "b", None, None);
        cache.store_at(&key, None, 1_000).unwrap();
        assert_eq!(cache.lookup_at(&key, 1_000), CacheLookup::KnownMissing);
        assert_eq!(
            cache.lookup_at(&key, 1_000 + NEGATIVE_TTL_SECS - 1),
            CacheLookup::KnownMissing
        );
        assert_eq!(
            cache.lookup_at(&key, 1_000 + NEGATIVE_TTL_SECS),
            CacheLookup::Miss
        );
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LyricsCache::new(dir.path());
        fs::write(dir.path().join("k.json"), b"garbage").unwrap();
        assert_eq!(cache.lookup("k"), CacheLookup::Miss);
    }
}
