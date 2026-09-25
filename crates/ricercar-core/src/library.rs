use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use walkdir::WalkDir;

use crate::meta::{file_uri, read_tags};

pub const SUPPORTED_EXTS: &[&str] = &[
    "flac", "wav", "aiff", "aif", "mp3", "ogg", "oga", "m4a", "mp4",
];

#[derive(Debug, Clone)]
pub struct Track {
    pub path: String,
    pub uri: String,
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AlbumKey {
    pub album: String,
    pub album_artist: Option<String>,
    pub track_count: u32,
    pub year: Option<i32>,
}

pub struct Library {
    conn: Arc<Mutex<Connection>>,
}

fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
            path TEXT PRIMARY KEY,
            title TEXT NOT NULL DEFAULT '',
            artist TEXT, album_artist TEXT, album TEXT,
            track INTEGER, disc INTEGER, year INTEGER,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            size INTEGER NOT NULL DEFAULT 0,
            mtime INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_album ON tracks(album, album_artist);",
    )
}

impl Library {
    pub fn open(path: &Path) -> rusqlite::Result<Library> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        schema(&conn)?;
        Ok(Library {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Library> {
        let conn = Connection::open_in_memory()?;
        schema(&conn)?;
        Ok(Library {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    /// Full scan of a root: upsert existing files, prune removed ones.
    pub fn scan_root(&self, root: &Path) -> usize {
        let mut found: BTreeSet<PathBuf> = BTreeSet::new();
        let mut added = 0usize;
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                continue;
            }
            found.insert(path.to_path_buf());
            if self.upsert(path) {
                added += 1;
            }
        }
        // prune tracks under this root that vanished
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT path FROM tracks WHERE path LIKE ?1 || '%'")
            .unwrap();
        let prefix = root.to_string_lossy().to_string();
        let existing: Vec<String> = stmt
            .query_map(params![prefix], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for p in existing {
            let pb = PathBuf::from(&p);
            if !found.contains(&pb) {
                conn.execute("DELETE FROM tracks WHERE path = ?1", params![p])
                    .unwrap();
            }
        }
        added
    }

    /// Insert or refresh one file. Returns true when new.
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
        let conn = self.conn();
        let changed = conn
            .execute(
                "INSERT INTO tracks (path, title, artist, album_artist, album, track, disc, year,
                    duration_ms, size, mtime)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(path) DO UPDATE SET title=?2, artist=?3, album_artist=?4, album=?5,
                    track=?6, disc=?7, year=?8, duration_ms=?9, size=?10, mtime=?11
                 WHERE size != ?10 OR mtime != ?11",
                params![
                    path.to_string_lossy(),
                    tags.title.unwrap_or_default(),
                    tags.artist,
                    tags.album_artist,
                    tags.album,
                    tags.track,
                    tags.disc,
                    tags.year,
                    tags.duration_ms as i64,
                    size,
                    mtime
                ],
            )
            .unwrap_or(0);
        changed > 0
    }

    pub fn remove(&self, path: &str) {
        let conn = self.conn();
        let _ = conn.execute("DELETE FROM tracks WHERE path = ?1", params![path]);
    }

    pub fn count(&self) -> u32 {
        let conn = self.conn();
        conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn has_path(&self, path: &str) -> bool {
        let conn = self.conn();
        conn.prepare("SELECT 1 FROM tracks WHERE path=?1")
            .and_then(|mut st| {
                let mut rows = st.query_map([path], |_| Ok(()))?;
                Ok(rows.next().is_some())
            })
            .unwrap_or(false)
    }

    pub fn albums(&self) -> Vec<AlbumKey> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(album,'(untitled)'), album_artist, COUNT(*), MIN(year)
                 FROM tracks GROUP BY COALESCE(album,''), album_artist
                 ORDER BY COALESCE(album,'') COLLATE NOCASE",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(AlbumKey {
                    album: r.get(0)?,
                    album_artist: r.get(1)?,
                    track_count: r.get(2)?,
                    year: r.get(3)?,
                })
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn album_tracks(&self, album: &str, album_artist: Option<&str>) -> Vec<Track> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT path,title,artist,album_artist,album,track,disc,year,duration_ms
                 FROM tracks WHERE COALESCE(album,'(untitled)')=?1
                   AND ((album_artist IS ?2) OR album_artist IS NULL)
                 ORDER BY COALESCE(disc,1), COALESCE(track,0), title COLLATE NOCASE",
            )
            .unwrap();
        let rows = stmt
            .query_map(params![album, album_artist], row_to_track)
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn search(&self, q: &str) -> Vec<Track> {
        let conn = self.conn();
        let like = format!("%{q}%");
        let mut stmt = conn
            .prepare(
                "SELECT path,title,artist,album_artist,album,track,disc,year,duration_ms
                 FROM tracks
                 WHERE title LIKE ?1 OR artist LIKE ?1 OR album LIKE ?1
                 ORDER BY artist COLLATE NOCASE, album COLLATE NOCASE, COALESCE(track,0)
                 LIMIT 200",
            )
            .unwrap();
        let rows = stmt.query_map(params![like], row_to_track).unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }
}

fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let path: String = r.get(0)?;
    Ok(Track {
        uri: file_uri(Path::new(&path)),
        path,
        title: r.get(1)?,
        artist: r.get(2)?,
        album_artist: r.get(3)?,
        album: r.get(4)?,
        track: r.get(5)?,
        disc: r.get(6)?,
        year: r.get(7)?,
        duration_ms: r.get::<_, i64>(8)? as u64,
    })
}
