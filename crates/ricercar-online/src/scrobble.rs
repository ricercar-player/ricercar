//! Service-agnostic scrobbling: when does a play count as a listen, and a
//! durable queue for listens that could not be submitted yet.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{OnlineError, Result, fsutil};

/// Metadata of a track as sent to scrobbling services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrobbleTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// Track length; `0` means unknown.
    pub duration_ms: u64,
    pub track_number: Option<u32>,
    /// MusicBrainz recording ID.
    pub mbid: Option<String>,
}

/// A qualifying listen, ready to submit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scrobble {
    pub track: ScrobbleTrack,
    /// When playback of this track started (Unix seconds, UTC).
    pub started_at_unix: i64,
}

/// Tracks shorter than this are never scrobbled (Last.fm/ListenBrainz rule).
pub const MIN_TRACK_MS: u64 = 30_000;
/// Listening this long always qualifies, whatever the track length.
pub const MAX_THRESHOLD_MS: u64 = 240_000;

/// Listened time after which a track qualifies, or `None` if it never can.
///
/// Standard rule: the track must be longer than 30 s, and qualifies once
/// `min(50 % of duration, 4 min)` has actually been heard. With an unknown
/// duration (`0`) we require the full 4 minutes, which implies > 30 s.
pub fn threshold_ms(duration_ms: u64) -> Option<u64> {
    match duration_ms {
        0 => Some(MAX_THRESHOLD_MS),
        d if d <= MIN_TRACK_MS => None,
        d => Some((d / 2).min(MAX_THRESHOLD_MS)),
    }
}

#[derive(Debug)]
struct Play {
    track: ScrobbleTrack,
    started_at_unix: i64,
    listened_ms: u64,
    emitted: bool,
}

/// Pure state machine deciding when the current play becomes a scrobble.
///
/// How to feed it:
/// - [`on_track_started`](Self::on_track_started) whenever playback of a
///   track begins, including when the same track is repeated or restarted
///   from the beginning (each call is a new play).
/// - [`on_progress`](Self::on_progress) with the **delta** of audio actually
///   played since the previous call, in ms. Do not feed while paused, and do
///   not count seek jumps: only wall-clock time spent rendering audio. Seeking
///   therefore neither helps nor hurts qualification.
/// - [`on_track_ended`](Self::on_track_ended) when playback stops or the
///   track finishes.
///
/// The scrobble is yielded by `on_progress` at the moment the threshold is
/// crossed (so a crash later in the track does not lose it), and at most once
/// per play.
#[derive(Debug, Default)]
pub struct ScrobbleTracker {
    current: Option<Play>,
}

impl ScrobbleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new play, abandoning any previous one.
    pub fn on_track_started(&mut self, track: ScrobbleTrack, now_unix: i64) {
        self.current = Some(Play {
            track,
            started_at_unix: now_unix,
            listened_ms: 0,
            emitted: false,
        });
    }

    /// Adds `played_ms` of actually-heard audio (a delta, see type docs).
    /// Returns the scrobble the first time the play qualifies.
    pub fn on_progress(&mut self, played_ms: u64) -> Option<Scrobble> {
        let play = self.current.as_mut()?;
        play.listened_ms = play.listened_ms.saturating_add(played_ms);
        if play.emitted {
            return None;
        }
        let threshold = threshold_ms(play.track.duration_ms)?;
        if play.listened_ms < threshold {
            return None;
        }
        play.emitted = true;
        Some(Scrobble {
            track: play.track.clone(),
            started_at_unix: play.started_at_unix,
        })
    }

    /// Ends the current play. Further progress is ignored until the next
    /// `on_track_started`.
    pub fn on_track_ended(&mut self) {
        self.current = None;
    }

    /// Track currently being played, e.g. for "now playing" notifications.
    pub fn current_track(&self) -> Option<&ScrobbleTrack> {
        self.current.as_ref().map(|p| &p.track)
    }

    /// Audio heard so far in the current play.
    pub fn listened_ms(&self) -> u64 {
        self.current.as_ref().map_or(0, |p| p.listened_ms)
    }

    /// Whether the current play already produced its scrobble.
    pub fn is_scrobbled(&self) -> bool {
        self.current.as_ref().is_some_and(|p| p.emitted)
    }
}

/// Persistent FIFO of scrobbles awaiting submission, stored as a JSON array.
///
/// Every mutation is written through atomically (temp file + rename), so the
/// queue survives crashes and power loss. Typical use:
/// `let batch = q.drain_batch(50)?;` submit it, and on a transient failure
/// `q.requeue(batch)?`.
#[derive(Debug)]
pub struct ScrobbleQueue {
    path: PathBuf,
    items: VecDeque<Scrobble>,
}

impl ScrobbleQueue {
    /// Opens (or lazily creates) the queue stored at `path`. A corrupt file
    /// is moved aside to `<path>.corrupt` rather than blocking scrobbling.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let items = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(items) => items,
                Err(e) => {
                    let mut aside = path.clone().into_os_string();
                    aside.push(".corrupt");
                    tracing::warn!(path = %path.display(), error = %e, "corrupt scrobble queue moved aside");
                    fs::rename(&path, aside)?;
                    VecDeque::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, items })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Appends a scrobble and persists.
    pub fn push(&mut self, scrobble: Scrobble) -> Result<()> {
        self.items.push_back(scrobble);
        self.save()
    }

    /// Removes and returns up to `max` oldest scrobbles, persisting the
    /// removal. Hand them back with [`requeue`](Self::requeue) on failure.
    pub fn drain_batch(&mut self, max: usize) -> Result<Vec<Scrobble>> {
        let n = max.min(self.items.len());
        if n == 0 {
            return Ok(Vec::new());
        }
        let batch: Vec<_> = self.items.drain(..n).collect();
        self.save()?;
        Ok(batch)
    }

    /// Puts a batch back at the front, preserving its order, and persists.
    pub fn requeue(&mut self, batch: Vec<Scrobble>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        for s in batch.into_iter().rev() {
            self.items.push_front(s);
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let json =
            serde_json::to_vec(&self.items).map_err(|e| OnlineError::Parse(e.to_string()))?;
        fsutil::write_atomic(&self.path, &json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(duration_ms: u64) -> ScrobbleTrack {
        ScrobbleTrack {
            artist: "Artist".into(),
            title: "Title".into(),
            album: Some("Album".into()),
            album_artist: None,
            duration_ms,
            track_number: Some(1),
            mbid: None,
        }
    }

    #[test]
    fn thresholds() {
        assert_eq!(threshold_ms(0), Some(240_000));
        assert_eq!(threshold_ms(29_000), None);
        assert_eq!(threshold_ms(30_000), None);
        assert_eq!(threshold_ms(30_001), Some(15_000));
        assert_eq!(threshold_ms(200_000), Some(100_000));
        assert_eq!(threshold_ms(480_000), Some(240_000));
        assert_eq!(threshold_ms(3_600_000), Some(240_000));
    }

    #[test]
    fn scrobbles_at_half_duration() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(200_000), 1_000);
        assert_eq!(t.on_progress(99_999), None);
        let s = t.on_progress(1).expect("qualifies at exactly 50%");
        assert_eq!(s.started_at_unix, 1_000);
        assert_eq!(s.track, track(200_000));
        assert!(t.is_scrobbled());
    }

    #[test]
    fn long_track_qualifies_after_four_minutes() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(20 * 60_000), 0);
        assert_eq!(t.on_progress(239_000), None);
        assert!(t.on_progress(1_000).is_some());
    }

    #[test]
    fn emitted_exactly_once_per_play() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(60_000), 0);
        let hits = (0..60).filter_map(|_| t.on_progress(1_000)).count();
        assert_eq!(hits, 1);
        assert_eq!(t.listened_ms(), 60_000);
    }

    #[test]
    fn short_tracks_never_scrobble() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(30_000), 0);
        assert_eq!(t.on_progress(30_000), None);
        assert_eq!(t.on_progress(1_000_000), None);
    }

    #[test]
    fn unknown_duration_needs_four_minutes() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(0), 0);
        assert_eq!(t.on_progress(239_999), None);
        assert!(t.on_progress(1).is_some());
    }

    #[test]
    fn pause_does_not_count() {
        // Caller stops feeding progress while paused: wall-clock time spent
        // paused is irrelevant, only the fed deltas matter.
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(100_000), 0);
        assert_eq!(t.on_progress(30_000), None);
        // ...paused for an hour, nothing fed...
        assert_eq!(t.on_progress(19_000), None);
        assert!(t.on_progress(1_000).is_some());
    }

    #[test]
    fn seeking_forward_does_not_qualify() {
        // Seek to 90% then listen 10 s: only 10 s actually heard.
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(300_000), 0);
        assert_eq!(t.on_progress(10_000), None);
        t.on_track_ended();
        assert!(!t.is_scrobbled());
    }

    #[test]
    fn seeking_backward_still_counts_listened_time() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(100_000), 0);
        assert_eq!(t.on_progress(30_000), None);
        // Seek back to 0 and listen another 20 s: 50 s heard in total.
        assert!(t.on_progress(20_000).is_some());
    }

    #[test]
    fn repeat_of_same_track_scrobbles_again() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(60_000), 100);
        assert!(t.on_progress(60_000).is_some());
        t.on_track_ended();
        t.on_track_started(track(60_000), 160);
        let second = t.on_progress(60_000).expect("new play");
        assert_eq!(second.started_at_unix, 160);
    }

    #[test]
    fn skipping_resets_progress() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(100_000), 0);
        assert_eq!(t.on_progress(40_000), None);
        // Skip to the next track without an explicit end.
        t.on_track_started(track(100_000), 40);
        assert_eq!(t.listened_ms(), 0);
        assert_eq!(t.on_progress(40_000), None);
        assert!(t.on_progress(10_000).is_some());
    }

    #[test]
    fn progress_after_end_or_before_start_is_ignored() {
        let mut t = ScrobbleTracker::new();
        assert_eq!(t.on_progress(1_000_000), None);
        t.on_track_started(track(60_000), 0);
        t.on_track_ended();
        assert_eq!(t.on_progress(1_000_000), None);
        assert!(t.current_track().is_none());
    }

    #[test]
    fn saturating_progress() {
        let mut t = ScrobbleTracker::new();
        t.on_track_started(track(60_000), 0);
        assert!(t.on_progress(u64::MAX).is_some());
        assert_eq!(t.on_progress(u64::MAX), None);
        assert_eq!(t.listened_ms(), u64::MAX);
    }

    fn scrobble(n: i64) -> Scrobble {
        Scrobble {
            track: track(100_000),
            started_at_unix: n,
        }
    }

    #[test]
    fn queue_persists_and_drains_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/queue.json");
        let mut q = ScrobbleQueue::open(&path).unwrap();
        assert!(q.is_empty());
        for i in 0..5 {
            q.push(scrobble(i)).unwrap();
        }
        let mut q = ScrobbleQueue::open(&path).unwrap();
        assert_eq!(q.len(), 5);
        let batch = q.drain_batch(3).unwrap();
        assert_eq!(
            batch.iter().map(|s| s.started_at_unix).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(ScrobbleQueue::open(&path).unwrap().len(), 2);

        q.requeue(batch).unwrap();
        let q2 = ScrobbleQueue::open(&path).unwrap();
        assert_eq!(
            q2.items
                .iter()
                .map(|s| s.started_at_unix)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn drain_more_than_available_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = ScrobbleQueue::open(dir.path().join("q.json")).unwrap();
        assert!(q.drain_batch(10).unwrap().is_empty());
        q.push(scrobble(1)).unwrap();
        assert_eq!(q.drain_batch(10).unwrap().len(), 1);
        assert!(q.is_empty());
        q.requeue(Vec::new()).unwrap();
        assert!(q.is_empty());
    }

    #[test]
    fn corrupt_queue_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        fs::write(&path, b"{not json").unwrap();
        let q = ScrobbleQueue::open(&path).unwrap();
        assert!(q.is_empty());
        assert!(dir.path().join("q.json.corrupt").exists());
    }
}
