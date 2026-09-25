//! SQLite music library: tracks, albums, artists, stats, playlists, search.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::meta::{TagInfo, file_uri, read_tags};

pub const SUPPORTED_EXTS: &[&str] = &[
    "flac", "wav", "aiff", "aif", "aifc", "mp3", "ogg", "oga", "opus", "m4a", "mp4", "alac",
];

const SCHEMA_VERSION: i32 = 2;

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub path: String,
    pub uri: String,
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub album_id: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub codec: Option<String>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub mb_album_id: Option<String>,
    pub favorite: bool,
    pub play_count: u32,
}

impl Track {
    pub fn is_hires(&self) -> bool {
        is_hires(self.sample_rate, self.bits)
    }
}

pub fn is_hires(rate: Option<u32>, bits: Option<u8>) -> bool {
    rate.is_some_and(|r| r > 48_000) || bits.is_some_and(|b| b > 16)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Album {
    pub id: String,
    pub title: String,
    /// Album artist, else the sole track artist, else "Various Artists".
    pub artist: String,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub track_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    pub added_at: i64,
    pub max_rate: Option<u32>,
    pub max_bits: Option<u8>,
    pub codec: Option<String>,
    /// A track of the album, to resolve the cover from.
    pub cover_path: String,
    pub dir: String,
    pub favorite: bool,
}

impl Album {
    pub fn is_hires(&self) -> bool {
        is_hires(self.max_rate, self.max_bits)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Artist {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
    pub cover_path: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Genre {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub track_count: u32,
    pub duration_ms: u64,
    pub updated_at: i64,
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty() && self.tracks.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlbumSort {
    #[default]
    Title,
    Artist,
    YearDesc,
    RecentlyAdded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSort {
    #[default]
    Artist,
    Title,
    Album,
    Duration,
    RecentlyAdded,
    MostPlayed,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryStats {
    pub tracks: u32,
    pub albums: u32,
    pub artists: u32,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub hires_tracks: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub total: usize,
}

/// Live scan progress, readable from any thread.
#[derive(Default)]
pub struct ScanProgress {
    pub running: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

pub struct Library {
    writer: Mutex<Connection>,
    /// Separate WAL reader so the UI never waits behind a scan transaction.
    reader: Option<Mutex<Connection>>,
    rev: AtomicU64,
    pub progress: ScanProgress,
}

fn schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < SCHEMA_VERSION {
        // v1 only held an index of files (no user data): rebuild it.
        conn.execute_batch(
            "DROP TABLE IF EXISTS tracks_fts;
             DROP TABLE IF EXISTS tracks;",
        )?;
    }
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            dir TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            artist TEXT, album_artist TEXT, album TEXT,
            album_id TEXT NOT NULL DEFAULT '',
            track INTEGER, disc INTEGER, year INTEGER, genre TEXT, composer TEXT,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            sample_rate INTEGER, bits INTEGER, channels INTEGER, bitrate INTEGER, codec TEXT,
            rg_track_gain REAL, rg_track_peak REAL, rg_album_gain REAL, rg_album_peak REAL,
            mb_album_id TEXT, mb_track_id TEXT,
            size INTEGER NOT NULL DEFAULT 0,
            mtime INTEGER NOT NULL DEFAULT 0,
            added_at INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_id);
         CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_aa ON tracks(album_artist COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_added ON tracks(added_at);

         CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
            title, artist, album, album_artist, genre, composer,
            content='tracks', content_rowid='id',
            tokenize='unicode61 remove_diacritics 2'
         );
         CREATE TRIGGER IF NOT EXISTS tracks_ai AFTER INSERT ON tracks BEGIN
            INSERT INTO tracks_fts(rowid, title, artist, album, album_artist, genre, composer)
            VALUES (new.id, new.title, new.artist, new.album, new.album_artist, new.genre, new.composer);
         END;
         CREATE TRIGGER IF NOT EXISTS tracks_ad AFTER DELETE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, album_artist, genre, composer)
            VALUES ('delete', old.id, old.title, old.artist, old.album, old.album_artist, old.genre, old.composer);
         END;
         CREATE TRIGGER IF NOT EXISTS tracks_au AFTER UPDATE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist, album, album_artist, genre, composer)
            VALUES ('delete', old.id, old.title, old.artist, old.album, old.album_artist, old.genre, old.composer);
            INSERT INTO tracks_fts(rowid, title, artist, album, album_artist, genre, composer)
            VALUES (new.id, new.title, new.artist, new.album, new.album_artist, new.genre, new.composer);
         END;

         CREATE TABLE IF NOT EXISTS stats (
            path TEXT PRIMARY KEY,
            play_count INTEGER NOT NULL DEFAULT 0,
            last_played INTEGER,
            favorite INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL,
            played_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_history_time ON history(played_at);
         CREATE TABLE IF NOT EXISTS album_favs (
            album_id TEXT PRIMARY KEY,
            added_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS playlists (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS playlist_items (
            playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
            pos INTEGER NOT NULL,
            path TEXT NOT NULL,
            PRIMARY KEY (playlist_id, pos)
         );",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable album identity: (album, album artist) when an album artist is
/// tagged, else (album, directory) so untagged compilations stay together.
pub fn album_id_for(tags: &TagInfo, path: &Path) -> String {
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let album = norm(tags.album.as_deref().unwrap_or(""));
    let key = match tags.album_artist.as_deref() {
        Some(aa) if !album.is_empty() => format!("{album}\u{1f}{}", norm(aa)),
        _ => format!("{album}\u{1f}dir:{dir}"),
    };
    format!("{:016x}", fnv(&key))
}

const TRACK_COLS: &str =
    "t.path, t.title, t.artist, t.album_artist, t.album, t.album_id, t.track, t.disc,
    t.year, t.genre, t.composer, t.duration_ms, t.sample_rate, t.bits, t.channels, t.bitrate,
    t.codec, t.rg_track_gain, t.rg_track_peak, t.rg_album_gain, t.rg_album_peak, t.mb_album_id,
    COALESCE(s.favorite, 0), COALESCE(s.play_count, 0)";

fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let path: String = r.get(0)?;
    Ok(Track {
        uri: file_uri(Path::new(&path)),
        path,
        title: r.get(1)?,
        artist: r.get(2)?,
        album_artist: r.get(3)?,
        album: r.get(4)?,
        album_id: r.get(5)?,
        track: r.get(6)?,
        disc: r.get(7)?,
        year: r.get(8)?,
        genre: r.get(9)?,
        composer: r.get(10)?,
        duration_ms: r.get::<_, i64>(11)? as u64,
        sample_rate: r.get(12)?,
        bits: r.get(13)?,
        channels: r.get(14)?,
        bitrate: r.get(15)?,
        codec: r.get(16)?,
        rg_track_gain: r.get(17)?,
        rg_track_peak: r.get(18)?,
        rg_album_gain: r.get(19)?,
        rg_album_peak: r.get(20)?,
        mb_album_id: r.get(21)?,
        favorite: r.get::<_, i64>(22)? != 0,
        play_count: r.get(23)?,
    })
}

const ALBUM_SELECT: &str = "SELECT t.album_id, MAX(t.album), MAX(t.album_artist), MAX(t.artist),
    COUNT(DISTINCT t.artist), MIN(t.year), MAX(t.genre), COUNT(*),
    COUNT(DISTINCT COALESCE(t.disc, 1)), SUM(t.duration_ms), MIN(t.added_at),
    MAX(t.sample_rate), MAX(t.bits), MAX(t.codec), MIN(t.path), MAX(t.dir),
    EXISTS(SELECT 1 FROM album_favs f WHERE f.album_id = t.album_id)
    FROM tracks t";

fn row_to_album(r: &rusqlite::Row<'_>) -> rusqlite::Result<Album> {
    let album: Option<String> = r.get(1)?;
    let album_artist: Option<String> = r.get(2)?;
    let artist: Option<String> = r.get(3)?;
    let distinct_artists: u32 = r.get(4)?;
    let dir: String = r.get(15)?;
    let title = album.filter(|a| !a.is_empty()).unwrap_or_else(|| {
        Path::new(&dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown album".into())
    });
    let artist = album_artist.unwrap_or_else(|| {
        if distinct_artists > 1 {
            "Various Artists".into()
        } else {
            artist.unwrap_or_else(|| "Unknown artist".into())
        }
    });
    Ok(Album {
        id: r.get(0)?,
        title,
        artist,
        year: r.get(5)?,
        genre: r.get(6)?,
        track_count: r.get(7)?,
        disc_count: r.get(8)?,
        duration_ms: r.get::<_, i64>(9)? as u64,
        added_at: r.get(10)?,
        max_rate: r.get(11)?,
        max_bits: r.get(12)?,
        codec: r.get(13)?,
        cover_path: r.get(14)?,
        dir,
        favorite: r.get(16)?,
    })
}

/// Turn free text into an FTS5 prefix query: `miles kind` → `"miles"* "kind"*`.
fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

struct Scanned {
    path: PathBuf,
    size: i64,
    mtime: i64,
}

impl Library {
    pub fn open(path: &Path) -> rusqlite::Result<Library> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let writer = Connection::open(path)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        writer.busy_timeout(std::time::Duration::from_secs(5))?;
        schema(&writer)?;
        let reader = Connection::open(path)?;
        reader.busy_timeout(std::time::Duration::from_secs(5))?;
        reader.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Library {
            writer: Mutex::new(writer),
            reader: Some(Mutex::new(reader)),
            rev: AtomicU64::new(1),
            progress: ScanProgress::default(),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Library> {
        let conn = Connection::open_in_memory()?;
        schema(&conn)?;
        Ok(Library {
            writer: Mutex::new(conn),
            reader: None,
            rev: AtomicU64::new(1),
            progress: ScanProgress::default(),
        })
    }

    fn w(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn r(&self) -> MutexGuard<'_, Connection> {
        match &self.reader {
            Some(r) => r.lock().unwrap_or_else(|e| e.into_inner()),
            None => self.w(),
        }
    }

    /// Bumped on every change; views compare it to know when to refresh.
    pub fn revision(&self) -> u64 {
        self.rev.load(Ordering::Relaxed)
    }

    fn touch(&self) {
        self.rev.fetch_add(1, Ordering::Relaxed);
    }

    fn query<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let conn = self.r();
        let res = conn.prepare_cached(sql).and_then(|mut st| {
            st.query_map(params, f)?
                .collect::<rusqlite::Result<Vec<T>>>()
        });
        res.unwrap_or_else(|e| {
            tracing::warn!("library query failed: {e}");
            #[cfg(test)]
            eprintln!("query failed: {e}\n{sql}");
            Vec::new()
        })
    }

    // ------------------------------------------------------------ scanning

    /// Incremental scan of several roots: only new or modified files are
    /// re-tagged (in parallel); vanished files are pruned.
    pub fn scan_roots(&self, roots: &[PathBuf]) -> ScanReport {
        if self.progress.running.swap(true, Ordering::SeqCst) {
            return ScanReport::default();
        }
        self.progress.done.store(0, Ordering::Relaxed);
        self.progress.total.store(0, Ordering::Relaxed);
        let report = self.scan_inner(roots);
        self.progress.running.store(false, Ordering::SeqCst);
        if report.added + report.updated + report.removed > 0 {
            self.touch();
        }
        report
    }

    fn scan_inner(&self, roots: &[PathBuf]) -> ScanReport {
        let known: HashMap<String, (i64, i64)> = self
            .query("SELECT path, size, mtime FROM tracks", [], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
            })
            .into_iter()
            .collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut todo: Vec<Scanned> = Vec::new();
        let mut report = ScanReport::default();
        let mut live_roots: Vec<String> = Vec::new();

        for root in roots {
            if !root.is_dir() {
                // Unmounted disk / offline NAS: never prune what we can't see.
                tracing::warn!("library root {} is not available", root.display());
                continue;
            }
            live_roots.push(root.to_string_lossy().into_owned());
            for entry in WalkDir::new(root).follow_links(true).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let ok_ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| SUPPORTED_EXTS.contains(&e.to_ascii_lowercase().as_str()));
                if !ok_ext {
                    continue;
                }
                let key = path.to_string_lossy().into_owned();
                let Ok(md) = entry.metadata() else { continue };
                let size = md.len() as i64;
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if known.get(&key) != Some(&(size, mtime)) {
                    todo.push(Scanned {
                        path: path.to_path_buf(),
                        size,
                        mtime,
                    });
                }
                seen.insert(key);
            }
        }
        report.total = seen.len();
        self.progress.total.store(todo.len(), Ordering::Relaxed);

        for chunk in todo.chunks(256) {
            let tagged: Vec<(&Scanned, TagInfo)> =
                chunk.par_iter().map(|s| (s, read_tags(&s.path))).collect();
            let mut conn = self.w();
            let Ok(tx) = conn.transaction() else { break };
            for (s, tags) in &tagged {
                match upsert_tags(&tx, &s.path, tags, s.size, s.mtime) {
                    Ok(true) => report.added += 1,
                    Ok(false) => report.updated += 1,
                    Err(e) => tracing::warn!("index {}: {e}", s.path.display()),
                }
            }
            let _ = tx.commit();
            drop(conn);
            self.progress.done.fetch_add(chunk.len(), Ordering::Relaxed);
            // Let views pick up big libraries progressively.
            self.touch();
        }

        let gone: Vec<&String> = known
            .keys()
            .filter(|p| !seen.contains(*p) && live_roots.iter().any(|r| p.starts_with(r.as_str())))
            .collect();
        if !gone.is_empty() {
            let mut conn = self.w();
            if let Ok(tx) = conn.transaction() {
                for p in &gone {
                    let _ = tx.execute("DELETE FROM tracks WHERE path = ?1", [p]);
                }
                let _ = tx.commit();
            }
            report.removed = gone.len();
        }
        report
    }

    /// Insert or refresh one file (watcher path). Returns true when new.
    pub fn upsert(&self, path: &Path) -> bool {
        let tags = read_tags(path);
        let md = std::fs::metadata(path).ok();
        let size = md.as_ref().map(|m| m.len() as i64).unwrap_or(0);
        let mtime = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let res = upsert_tags(&self.w(), path, &tags, size, mtime);
        self.touch();
        res.unwrap_or(false)
    }

    pub fn remove(&self, path: &str) {
        let _ = self
            .w()
            .execute("DELETE FROM tracks WHERE path = ?1", [path]);
        self.touch();
    }

    /// Drop every track outside the given roots (after roots were removed
    /// from the settings).
    pub fn retain_roots(&self, roots: &[PathBuf]) -> usize {
        let paths: Vec<String> = self.query("SELECT path FROM tracks", [], |r| r.get(0));
        let roots: Vec<String> = roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        let mut n = 0;
        let mut conn = self.w();
        if let Ok(tx) = conn.transaction() {
            for p in paths
                .iter()
                .filter(|p| !roots.iter().any(|r| p.starts_with(r.as_str())))
            {
                n += tx
                    .execute("DELETE FROM tracks WHERE path = ?1", [p])
                    .unwrap_or(0);
            }
            let _ = tx.commit();
        }
        drop(conn);
        if n > 0 {
            self.touch();
        }
        n
    }

    // ------------------------------------------------------------ lookups

    pub fn count(&self) -> u32 {
        self.r()
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn has_path(&self, path: &str) -> bool {
        self.r()
            .query_row("SELECT 1 FROM tracks WHERE path = ?1", [path], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn track(&self, path: &str) -> Option<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path WHERE t.path = ?1"
            ),
            [path],
            row_to_track,
        )
        .into_iter()
        .next()
    }

    pub fn stats(&self) -> LibraryStats {
        self.r()
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT album_id),
                    COUNT(DISTINCT COALESCE(album_artist, artist)),
                    COALESCE(SUM(duration_ms), 0), COALESCE(SUM(size), 0),
                    SUM(CASE WHEN sample_rate > 48000 OR bits > 16 THEN 1 ELSE 0 END)
                 FROM tracks",
                [],
                |r| {
                    Ok(LibraryStats {
                        tracks: r.get(0)?,
                        albums: r.get(1)?,
                        artists: r.get(2)?,
                        duration_ms: r.get::<_, i64>(3)? as u64,
                        size_bytes: r.get::<_, i64>(4)? as u64,
                        hires_tracks: r.get::<_, Option<u32>>(5)?.unwrap_or(0),
                    })
                },
            )
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ albums

    pub fn albums(&self, sort: AlbumSort) -> Vec<Album> {
        let order = match sort {
            AlbumSort::Title => "MAX(t.album) COLLATE NOCASE",
            AlbumSort::Artist => {
                "COALESCE(MAX(t.album_artist), MAX(t.artist)) COLLATE NOCASE, MIN(t.year), MAX(t.album) COLLATE NOCASE"
            }
            AlbumSort::YearDesc => "MIN(t.year) DESC, MAX(t.album) COLLATE NOCASE",
            AlbumSort::RecentlyAdded => "MIN(t.added_at) DESC, MAX(t.album) COLLATE NOCASE",
        };
        self.query(
            &format!("{ALBUM_SELECT} GROUP BY t.album_id ORDER BY {order}"),
            [],
            row_to_album,
        )
    }

    pub fn album(&self, id: &str) -> Option<Album> {
        self.query(
            &format!("{ALBUM_SELECT} WHERE t.album_id = ?1 GROUP BY t.album_id"),
            [id],
            row_to_album,
        )
        .into_iter()
        .next()
    }

    fn albums_by_ids(&self, ids: &[String]) -> Vec<Album> {
        if ids.is_empty() {
            return Vec::new();
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut found: HashMap<String, Album> = self
            .query(
                &format!("{ALBUM_SELECT} WHERE t.album_id IN ({marks}) GROUP BY t.album_id"),
                params_from_iter(ids.iter()),
                row_to_album,
            )
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
        ids.iter().filter_map(|id| found.remove(id)).collect()
    }

    pub fn album_tracks(&self, album_id: &str) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 WHERE t.album_id = ?1
                 ORDER BY COALESCE(t.disc, 1), COALESCE(t.track, 0), t.path"
            ),
            [album_id],
            row_to_track,
        )
    }

    pub fn recently_added_albums(&self, limit: usize) -> Vec<Album> {
        self.query(
            &format!(
                "{ALBUM_SELECT} GROUP BY t.album_id ORDER BY MIN(t.added_at) DESC, MAX(t.album) LIMIT ?1"
            ),
            [limit as i64],
            row_to_album,
        )
    }

    pub fn recently_played_albums(&self, limit: usize) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT t.album_id FROM history h JOIN tracks t ON t.path = h.path
             GROUP BY t.album_id ORDER BY MAX(h.played_at) DESC LIMIT ?1",
            [limit as i64],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn most_played_albums(&self, limit: usize) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT t.album_id FROM stats s JOIN tracks t ON t.path = s.path
             WHERE s.play_count > 0
             GROUP BY t.album_id ORDER BY SUM(s.play_count) DESC LIMIT ?1",
            [limit as i64],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn favorite_albums(&self) -> Vec<Album> {
        let ids: Vec<String> = self.query(
            "SELECT album_id FROM album_favs ORDER BY added_at DESC",
            [],
            |r| r.get(0),
        );
        self.albums_by_ids(&ids)
    }

    pub fn set_album_favorite(&self, album_id: &str, fav: bool) {
        let conn = self.w();
        let _ = if fav {
            conn.execute(
                "INSERT OR IGNORE INTO album_favs (album_id, added_at) VALUES (?1, ?2)",
                params![album_id, now_unix()],
            )
        } else {
            conn.execute("DELETE FROM album_favs WHERE album_id = ?1", [album_id])
        };
        drop(conn);
        self.touch();
    }

    // ------------------------------------------------------------ artists & genres

    pub fn artists(&self) -> Vec<Artist> {
        self.query(
            "SELECT COALESCE(album_artist, artist) AS name, COUNT(DISTINCT album_id), COUNT(*), MIN(path)
             FROM tracks WHERE name IS NOT NULL AND name != ''
             GROUP BY name COLLATE NOCASE ORDER BY name COLLATE NOCASE",
            [],
            |r| {
                Ok(Artist {
                    name: r.get(0)?,
                    album_count: r.get(1)?,
                    track_count: r.get(2)?,
                    cover_path: r.get(3)?,
                })
            },
        )
    }

    /// (own albums, albums the artist appears on).
    pub fn artist_albums(&self, name: &str) -> (Vec<Album>, Vec<Album>) {
        let own = self.query(
            &format!(
                "{ALBUM_SELECT} WHERE t.album_id IN (
                    SELECT album_id FROM tracks WHERE COALESCE(album_artist, artist) = ?1 COLLATE NOCASE)
                 GROUP BY t.album_id ORDER BY MIN(t.year) DESC, MAX(t.album)"
            ),
            [name],
            row_to_album,
        );
        let own_ids: HashSet<&str> = own.iter().map(|a| a.id.as_str()).collect();
        let appears = self
            .query(
                &format!(
                    "{ALBUM_SELECT} WHERE t.album_id IN (
                        SELECT album_id FROM tracks WHERE artist = ?1 COLLATE NOCASE
                           OR composer = ?1 COLLATE NOCASE)
                     GROUP BY t.album_id ORDER BY MIN(t.year) DESC, MAX(t.album)"
                ),
                [name],
                row_to_album,
            )
            .into_iter()
            .filter(|a| !own_ids.contains(a.id.as_str()))
            .collect();
        (own, appears)
    }

    /// Most played tracks by an artist (artist page "popular" list).
    pub fn artist_top_tracks(&self, name: &str, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 WHERE t.artist = ?1 COLLATE NOCASE OR t.album_artist = ?1 COLLATE NOCASE
                 ORDER BY COALESCE(s.play_count, 0) DESC, COALESCE(s.favorite, 0) DESC, t.year DESC
                 LIMIT ?2"
            ),
            params![name, limit as i64],
            row_to_track,
        )
    }

    pub fn genres(&self) -> Vec<Genre> {
        self.query(
            "SELECT genre, COUNT(DISTINCT album_id), COUNT(*) FROM tracks
             WHERE genre IS NOT NULL AND genre != ''
             GROUP BY genre COLLATE NOCASE ORDER BY COUNT(*) DESC",
            [],
            |r| {
                Ok(Genre {
                    name: r.get(0)?,
                    album_count: r.get(1)?,
                    track_count: r.get(2)?,
                })
            },
        )
    }

    pub fn genre_albums(&self, genre: &str) -> Vec<Album> {
        self.query(
            &format!(
                "{ALBUM_SELECT} WHERE t.album_id IN (SELECT album_id FROM tracks WHERE genre = ?1 COLLATE NOCASE)
                 GROUP BY t.album_id ORDER BY COALESCE(MAX(t.album_artist), MAX(t.artist)) COLLATE NOCASE"
            ),
            [genre],
            row_to_album,
        )
    }

    // ------------------------------------------------------------ tracks

    pub fn tracks(&self, sort: TrackSort) -> Vec<Track> {
        let order = match sort {
            TrackSort::Artist => {
                "COALESCE(t.album_artist, t.artist) COLLATE NOCASE, t.year, t.album_id, COALESCE(t.disc,1), COALESCE(t.track,0)"
            }
            TrackSort::Title => "t.title COLLATE NOCASE",
            TrackSort::Album => "t.album COLLATE NOCASE, COALESCE(t.disc,1), COALESCE(t.track,0)",
            TrackSort::Duration => "t.duration_ms DESC",
            TrackSort::RecentlyAdded => "t.added_at DESC, t.path",
            TrackSort::MostPlayed => "COALESCE(s.play_count,0) DESC, t.title COLLATE NOCASE",
        };
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path ORDER BY {order}"
            ),
            [],
            row_to_track,
        )
    }

    pub fn favorite_tracks(&self) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.favorite = 1 ORDER BY COALESCE(t.album_artist, t.artist) COLLATE NOCASE, t.album, t.track"
            ),
            [],
            row_to_track,
        )
    }

    pub fn most_played_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.play_count > 0 ORDER BY s.play_count DESC, s.last_played DESC LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    pub fn recently_played_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t JOIN stats s ON s.path = t.path
                 WHERE s.last_played IS NOT NULL ORDER BY s.last_played DESC LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    /// Random tracks (for a "shuffle all" / radio-like mix).
    pub fn random_tracks(&self, limit: usize) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks t LEFT JOIN stats s ON s.path = t.path
                 ORDER BY RANDOM() LIMIT ?1"
            ),
            [limit as i64],
            row_to_track,
        )
    }

    pub fn set_favorite(&self, path: &str, fav: bool) {
        let _ = self.w().execute(
            "INSERT INTO stats (path, favorite) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET favorite = ?2",
            params![path, fav as i64],
        );
        self.touch();
    }

    pub fn is_favorite(&self, path: &str) -> bool {
        self.r()
            .query_row("SELECT favorite FROM stats WHERE path = ?1", [path], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn record_play(&self, path: &str) {
        let now = now_unix();
        let conn = self.w();
        let _ = conn.execute(
            "INSERT INTO stats (path, play_count, last_played) VALUES (?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET play_count = play_count + 1, last_played = ?2",
            params![path, now],
        );
        let _ = conn.execute(
            "INSERT INTO history (path, played_at) VALUES (?1, ?2)",
            params![path, now],
        );
        drop(conn);
        self.touch();
    }

    // ------------------------------------------------------------ search

    pub fn search(&self, q: &str) -> SearchResults {
        let Some(fq) = fts_query(q) else {
            return SearchResults::default();
        };
        let tracks = self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM tracks_fts f JOIN tracks t ON t.id = f.rowid
                 LEFT JOIN stats s ON s.path = t.path
                 WHERE tracks_fts MATCH ?1 ORDER BY bm25(tracks_fts, 10.0, 5.0, 4.0, 4.0, 1.0, 2.0) LIMIT 200"
            ),
            [&fq],
            row_to_track,
        );
        let album_ids: Vec<String> = self.query(
            "WITH m AS MATERIALIZED (
                SELECT rowid, bm25(tracks_fts, 1.0, 5.0, 10.0, 8.0, 1.0, 2.0) AS score
                FROM tracks_fts WHERE tracks_fts MATCH ?1)
             SELECT t.album_id FROM m JOIN tracks t ON t.id = m.rowid
             GROUP BY t.album_id ORDER BY MIN(m.score) LIMIT 40",
            [format!("{{album album_artist artist}} : ({fq})")],
            |r| r.get(0),
        );
        let artist_names: Vec<String> = self.query(
            "SELECT COALESCE(t.album_artist, t.artist) AS n FROM tracks_fts f JOIN tracks t ON t.id = f.rowid
             WHERE tracks_fts MATCH ?1 AND n IS NOT NULL
             GROUP BY n COLLATE NOCASE ORDER BY COUNT(*) DESC LIMIT 12",
            [format!("{{artist album_artist}} : ({fq})")],
            |r| r.get(0),
        );
        let wanted: HashSet<String> = artist_names.iter().map(|n| n.to_lowercase()).collect();
        let mut artists: Vec<Artist> = self
            .artists()
            .into_iter()
            .filter(|a| wanted.contains(&a.name.to_lowercase()))
            .collect();
        artists.sort_by_key(|a| {
            artist_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&a.name))
                .unwrap_or(usize::MAX)
        });
        SearchResults {
            artists,
            albums: self.albums_by_ids(&album_ids),
            tracks,
        }
    }

    // ------------------------------------------------------------ playlists

    pub fn playlists(&self) -> Vec<Playlist> {
        self.query(
            "SELECT p.id, p.name, COUNT(t.path), COALESCE(SUM(t.duration_ms), 0), p.updated_at,
                (SELECT i.path FROM playlist_items i WHERE i.playlist_id = p.id ORDER BY i.pos LIMIT 1)
             FROM playlists p
             LEFT JOIN playlist_items i ON i.playlist_id = p.id
             LEFT JOIN tracks t ON t.path = i.path
             GROUP BY p.id ORDER BY p.name COLLATE NOCASE",
            [],
            |r| {
                Ok(Playlist {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    track_count: r.get(2)?,
                    duration_ms: r.get::<_, i64>(3)? as u64,
                    updated_at: r.get(4)?,
                    cover_path: r.get(5)?,
                })
            },
        )
    }

    pub fn playlist(&self, id: i64) -> Option<Playlist> {
        self.playlists().into_iter().find(|p| p.id == id)
    }

    pub fn create_playlist(&self, name: &str) -> i64 {
        let conn = self.w();
        let now = now_unix();
        let _ = conn.execute(
            "INSERT INTO playlists (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
            params![name, now],
        );
        let id = conn.last_insert_rowid();
        drop(conn);
        self.touch();
        id
    }

    pub fn rename_playlist(&self, id: i64, name: &str) {
        let _ = self.w().execute(
            "UPDATE playlists SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now_unix()],
        );
        self.touch();
    }

    pub fn delete_playlist(&self, id: i64) {
        let conn = self.w();
        let _ = conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id]);
        let _ = conn.execute("DELETE FROM playlists WHERE id = ?1", [id]);
        drop(conn);
        self.touch();
    }

    pub fn playlist_tracks(&self, id: i64) -> Vec<Track> {
        self.query(
            &format!(
                "SELECT {TRACK_COLS} FROM playlist_items i JOIN tracks t ON t.path = i.path
                 LEFT JOIN stats s ON s.path = t.path
                 WHERE i.playlist_id = ?1 ORDER BY i.pos"
            ),
            [id],
            row_to_track,
        )
    }

    fn playlist_paths(&self, id: i64) -> Vec<String> {
        self.query(
            "SELECT path FROM playlist_items WHERE playlist_id = ?1 ORDER BY pos",
            [id],
            |r| r.get(0),
        )
    }

    /// Replace the whole item list (simplest correct way to reorder/remove).
    pub fn set_playlist_paths(&self, id: i64, paths: &[String]) {
        let mut conn = self.w();
        if let Ok(tx) = conn.transaction() {
            let _ = tx.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id]);
            for (pos, p) in paths.iter().enumerate() {
                let _ = tx.execute(
                    "INSERT INTO playlist_items (playlist_id, pos, path) VALUES (?1, ?2, ?3)",
                    params![id, pos as i64, p],
                );
            }
            let _ = tx.execute(
                "UPDATE playlists SET updated_at = ?2 WHERE id = ?1",
                params![id, now_unix()],
            );
            let _ = tx.commit();
        }
        drop(conn);
        self.touch();
    }

    pub fn add_to_playlist(&self, id: i64, paths: &[String]) {
        let mut all = self.playlist_paths(id);
        all.extend(paths.iter().cloned());
        self.set_playlist_paths(id, &all);
    }

    /// Remove the items at the given positions (as returned by `playlist_tracks`
    /// order, which skips files no longer in the library).
    pub fn remove_from_playlist(&self, id: i64, positions: &[usize]) {
        let visible: Vec<String> = self
            .playlist_tracks(id)
            .into_iter()
            .map(|t| t.path)
            .collect();
        let drop_paths: Vec<&String> = positions.iter().filter_map(|&i| visible.get(i)).collect();
        let mut remaining = self.playlist_paths(id);
        for p in drop_paths {
            if let Some(i) = remaining.iter().position(|x| x == p) {
                remaining.remove(i);
            }
        }
        self.set_playlist_paths(id, &remaining);
    }

    pub fn move_in_playlist(&self, id: i64, from: usize, to: usize) {
        let mut paths = self.playlist_paths(id);
        if from < paths.len() && to < paths.len() {
            let p = paths.remove(from);
            paths.insert(to, p);
            self.set_playlist_paths(id, &paths);
        }
    }

    pub fn export_m3u(&self, id: i64, dest: &Path) -> std::io::Result<()> {
        let mut out = String::from("#EXTM3U\n");
        for t in self.playlist_tracks(id) {
            out.push_str(&format!(
                "#EXTINF:{},{} - {}\n{}\n",
                t.duration_ms / 1000,
                t.artist.as_deref().unwrap_or(""),
                t.title,
                t.path
            ));
        }
        std::fs::write(dest, out)
    }

    /// Import an M3U/M3U8 file; entries not in the library are skipped.
    pub fn import_m3u(&self, src: &Path) -> std::io::Result<i64> {
        let text = std::fs::read_to_string(src)?;
        let base = src.parent().unwrap_or(Path::new("/"));
        let paths: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let p = crate::meta::uri_to_path(l).unwrap_or_else(|| PathBuf::from(l));
                if p.is_absolute() { p } else { base.join(p) }
            })
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| self.has_path(p))
            .collect();
        let name = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Imported".into());
        let id = self.create_playlist(&name);
        self.set_playlist_paths(id, &paths);
        Ok(id)
    }
}

/// Returns Ok(true) when the row is new.
fn upsert_tags(
    conn: &Connection,
    path: &Path,
    tags: &TagInfo,
    size: i64,
    mtime: i64,
) -> rusqlite::Result<bool> {
    let key = path.to_string_lossy();
    let existed = conn
        .query_row("SELECT 1 FROM tracks WHERE path = ?1", [&key], |_| Ok(()))
        .optional()?
        .is_some();
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    conn.execute(
        "INSERT INTO tracks (path, dir, title, artist, album_artist, album, album_id, track, disc,
            year, genre, composer, duration_ms, sample_rate, bits, channels, bitrate, codec,
            rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak, mb_album_id, mb_track_id,
            size, mtime, added_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)
         ON CONFLICT(path) DO UPDATE SET dir=?2, title=?3, artist=?4, album_artist=?5, album=?6,
            album_id=?7, track=?8, disc=?9, year=?10, genre=?11, composer=?12, duration_ms=?13,
            sample_rate=?14, bits=?15, channels=?16, bitrate=?17, codec=?18, rg_track_gain=?19,
            rg_track_peak=?20, rg_album_gain=?21, rg_album_peak=?22, mb_album_id=?23,
            mb_track_id=?24, size=?25, mtime=?26",
        params![
            key,
            dir,
            tags.title.clone().unwrap_or_default(),
            tags.artist,
            tags.album_artist,
            tags.album,
            album_id_for(tags, path),
            tags.track,
            tags.disc,
            tags.year,
            tags.genre,
            tags.composer,
            tags.duration_ms as i64,
            tags.sample_rate,
            tags.bits,
            tags.channels,
            tags.bitrate,
            tags.codec,
            tags.rg_track_gain,
            tags.rg_track_peak,
            tags.rg_album_gain,
            tags.rg_album_peak,
            tags.mb_album_id,
            tags.mb_track_id,
            size,
            mtime,
            now_unix(),
        ],
    )?;
    Ok(!existed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(title: &str, artist: &str, album: &str, aa: Option<&str>, n: u32) -> TagInfo {
        TagInfo {
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some(album.into()),
            album_artist: aa.map(Into::into),
            track: Some(n),
            duration_ms: 180_000,
            sample_rate: Some(96_000),
            bits: Some(24),
            genre: Some("Jazz".into()),
            ..Default::default()
        }
    }

    fn lib_with(rows: &[(&str, TagInfo)]) -> Library {
        let lib = Library::in_memory().unwrap();
        for (p, t) in rows {
            upsert_tags(&lib.w(), Path::new(p), t, 1, 1).unwrap();
        }
        lib
    }

    #[test]
    fn albums_group_by_album_artist_or_dir() {
        let lib = lib_with(&[
            (
                "/m/kob/1.flac",
                tags(
                    "So What",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    1,
                ),
            ),
            (
                "/m/kob/2.flac",
                tags(
                    "Freddie",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    2,
                ),
            ),
            ("/m/comp/1.flac", tags("A", "X", "Hits", None, 1)),
            ("/m/comp/2.flac", tags("B", "Y", "Hits", None, 2)),
            ("/m/other/1.flac", tags("C", "Z", "Hits", None, 1)),
        ]);
        let albums = lib.albums(AlbumSort::Title);
        assert_eq!(albums.len(), 3);
        let comp = albums.iter().find(|a| a.dir == "/m/comp").unwrap();
        assert_eq!(comp.artist, "Various Artists");
        assert_eq!(comp.track_count, 2);
        let kob = albums.iter().find(|a| a.title == "Kind of Blue").unwrap();
        assert!(kob.is_hires());
        let tracks = lib.album_tracks(&kob.id);
        assert_eq!(tracks[0].title, "So What");
        assert_eq!(lib.stats().albums, 3);
    }

    #[test]
    fn fts_search_prefix_and_diacritics() {
        let lib = lib_with(&[
            (
                "/m/a/1.flac",
                tags("Jóga", "Björk", "Homogenic", Some("Björk"), 1),
            ),
            (
                "/m/b/1.flac",
                tags(
                    "So What",
                    "Miles Davis",
                    "Kind of Blue",
                    Some("Miles Davis"),
                    1,
                ),
            ),
        ]);
        let r = lib.search("bjork");
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.artists[0].name, "Björk");
        let r = lib.search("mil kind");
        assert_eq!(r.albums.len(), 1);
        assert_eq!(r.albums[0].title, "Kind of Blue");
        assert!(lib.search("  ").is_empty());
        // update keeps FTS in sync
        upsert_tags(
            &lib.w(),
            Path::new("/m/b/1.flac"),
            &tags(
                "Blue in Green",
                "Miles Davis",
                "Kind of Blue",
                Some("Miles Davis"),
                3,
            ),
            2,
            2,
        )
        .unwrap();
        assert!(lib.search("what").tracks.is_empty());
        assert_eq!(lib.search("green").tracks.len(), 1);
    }

    #[test]
    fn stats_favorites_and_history() {
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
        ]);
        let rev = lib.revision();
        lib.record_play("/m/a/2.flac");
        lib.record_play("/m/a/2.flac");
        lib.set_favorite("/m/a/1.flac", true);
        assert!(lib.revision() > rev);
        assert_eq!(lib.most_played_tracks(5)[0].play_count, 2);
        assert_eq!(lib.favorite_tracks()[0].title, "One");
        assert!(lib.track("/m/a/1.flac").unwrap().favorite);
        assert_eq!(lib.recently_played_albums(5).len(), 1);
        let id = lib.albums(AlbumSort::Title)[0].id.clone();
        lib.set_album_favorite(&id, true);
        assert!(lib.album(&id).unwrap().favorite);
        assert_eq!(lib.favorite_albums().len(), 1);
    }

    #[test]
    fn playlists_crud_and_m3u() {
        let dir = tempfile::tempdir().unwrap();
        let lib = lib_with(&[
            ("/m/a/1.flac", tags("One", "A", "Al", Some("A"), 1)),
            ("/m/a/2.flac", tags("Two", "A", "Al", Some("A"), 2)),
            ("/m/a/3.flac", tags("Three", "A", "Al", Some("A"), 3)),
        ]);
        let id = lib.create_playlist("Mix");
        lib.add_to_playlist(
            id,
            &[
                "/m/a/3.flac".into(),
                "/m/a/1.flac".into(),
                "/m/a/2.flac".into(),
            ],
        );
        lib.move_in_playlist(id, 0, 2);
        let titles: Vec<String> = lib
            .playlist_tracks(id)
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, ["One", "Two", "Three"]);
        lib.remove_from_playlist(id, &[1]);
        assert_eq!(lib.playlist(id).unwrap().track_count, 2);

        let m3u = dir.path().join("mix.m3u8");
        lib.export_m3u(id, &m3u).unwrap();
        let id2 = lib.import_m3u(&m3u).unwrap();
        assert_eq!(lib.playlist_tracks(id2).len(), 2);
        lib.rename_playlist(id2, "Copy");
        lib.delete_playlist(id);
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].name, "Copy");
    }

    #[test]
    fn scan_is_incremental_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("al")).unwrap();
        std::fs::write(root.join("al/a.flac"), b"not really flac").unwrap();
        std::fs::write(root.join("al/b.mp3"), b"nope").unwrap();
        std::fs::write(root.join("al/cover.jpg"), b"").unwrap();
        let lib = Library::open(&dir.path().join("db/lib.db")).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.total), (2, 2));
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!((r.added, r.updated, r.removed), (0, 0, 0));
        std::fs::remove_file(root.join("al/b.mp3")).unwrap();
        let r = lib.scan_roots(std::slice::from_ref(&root));
        assert_eq!(r.removed, 1);
        // a missing root never prunes
        let r = lib.scan_roots(&[root.join("gone")]);
        assert_eq!(r.removed, 0);
        assert_eq!(lib.count(), 1);
        // untagged file falls back to file name / dir name
        let a = &lib.albums(AlbumSort::Title)[0];
        assert_eq!(a.title, "al");
        assert_eq!(lib.retain_roots(&[]), 1);
    }
}
