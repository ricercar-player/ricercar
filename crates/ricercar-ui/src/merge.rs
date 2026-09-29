//! Merging the local library with plugin libraries: sort keys that follow
//! the pages' orders, and the merge of two sorted lists (the local list
//! comes sorted from SQLite and is never re-sorted, so its pages keep
//! lining up).

use ricercar_core::library::{AlbumSort, Track, TrackSort};

/// Comparable sort key.
pub type Key = (String, i64, String, i64, i64);

fn lower(s: Option<&str>) -> String {
    s.unwrap_or("").trim().to_lowercase()
}

/// Album order of the Albums page. Plugin albums have no "added" date
/// (`added` 0): they come after local ones in that order.
pub fn album_key(sort: AlbumSort, title: &str, artist: &str, year: Option<i32>, added: i64) -> Key {
    let title = lower(Some(title));
    let year = year.unwrap_or(0) as i64;
    match sort {
        AlbumSort::Title => (title, 0, String::new(), 0, 0),
        AlbumSort::Artist => (lower(Some(artist)), year, title, 0, 0),
        AlbumSort::YearDesc => (String::new(), -year, title, 0, 0),
        AlbumSort::RecentlyAdded => (String::new(), -added, title, 0, 0),
    }
}

/// Track order of the Tracks page (plugin tracks have no "added" date and
/// no local play count).
pub fn track_key(t: &Track, sort: TrackSort) -> Key {
    let title = lower(Some(&t.title));
    let n = |v: Option<u32>| v.unwrap_or(0) as i64;
    match sort {
        TrackSort::Artist => (
            lower(t.album_artist.as_deref().or(t.artist.as_deref())),
            t.year.unwrap_or(0) as i64,
            lower(t.album.as_deref()),
            n(t.disc).max(1),
            n(t.track),
        ),
        TrackSort::Title => (title, 0, String::new(), 0, 0),
        TrackSort::Album => (
            lower(t.album.as_deref()),
            0,
            String::new(),
            n(t.disc).max(1),
            n(t.track),
        ),
        TrackSort::Duration => (String::new(), -(t.duration_ms as i64), title, 0, 0),
        TrackSort::RecentlyAdded => (String::new(), t.is_plugin() as i64, String::new(), 0, 0),
        TrackSort::MostPlayed => (String::new(), -(t.play_count as i64), title, 0, 0),
    }
}

/// Merge two lists each sorted by `key`; on equal keys `a` comes first.
/// The first n items depend only on the first n of each list, so a page
/// merged from a prefix matches the start of the full merge.
pub fn merge_sorted<T>(a: Vec<T>, b: Vec<T>, key: impl Fn(&T) -> Key) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => key(x) <= key(y),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return out,
        };
        out.extend(if take_a { a.next() } else { b.next() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(title: &str, artist: &str, plugin: bool) -> Track {
        Track {
            path: if plugin {
                format!("plugin://p/{title}")
            } else {
                format!("/m/{title}")
            },
            title: title.into(),
            artist: Some(artist.into()),
            ..Default::default()
        }
    }

    #[test]
    fn merge_is_stable_and_prefix_consistent() {
        let local: Vec<Track> = ["alpha", "delta", "golf"]
            .iter()
            .map(|s| t(s, "x", false))
            .collect();
        let mut plugin: Vec<Track> = ["echo", "bravo", "alpha"]
            .iter()
            .map(|s| t(s, "x", true))
            .collect();
        plugin.sort_by_key(|x| track_key(x, TrackSort::Title));
        let key = |x: &Track| track_key(x, TrackSort::Title);
        let all = merge_sorted(local.clone(), plugin.clone(), key);
        let order: Vec<(&str, bool)> = all
            .iter()
            .map(|x| (x.title.as_str(), x.is_plugin()))
            .collect();
        assert_eq!(
            order,
            [
                ("alpha", false),
                ("alpha", true),
                ("bravo", true),
                ("delta", false),
                ("echo", true),
                ("golf", false)
            ]
        );
        // A page merged from the first 2 local tracks is the start of the whole.
        let first = merge_sorted(local[..2].to_vec(), plugin, key);
        assert_eq!(
            first[..3].iter().map(|x| &x.title).collect::<Vec<_>>(),
            ["alpha", "alpha", "bravo"]
        );
    }

    #[test]
    fn keys_follow_page_orders() {
        let a = album_key(AlbumSort::YearDesc, "B", "x", Some(2020), 0);
        let b = album_key(AlbumSort::YearDesc, "A", "x", Some(1999), 0);
        assert!(a < b);
        let local = album_key(AlbumSort::RecentlyAdded, "Z", "x", None, 1_700_000_000);
        let plugin = album_key(AlbumSort::RecentlyAdded, "A", "x", None, 0);
        assert!(local < plugin);
        let mut x = t("x", "Artist", true);
        x.album_artist = Some("Band".into());
        assert_eq!(track_key(&x, TrackSort::Artist).0, "band");
        assert!(
            track_key(&t("a", "z", false), TrackSort::RecentlyAdded)
                < track_key(&x, TrackSort::RecentlyAdded)
        );
    }
}
