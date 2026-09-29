//! Interface state kept between runs (`$XDG_DATA_HOME/ricercar/ui-state.json`):
//! window geometry, last page, sorts and filters, queue drawer, recent
//! searches. Distinct from the config file: nothing here is a setting.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Page;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Logical size.
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// Physical position; X11 only (Wayland never reports it).
    pub x: Option<i32>,
    pub y: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub window: Option<WindowState>,
    pub page: String,
    pub page_arg: String,
    pub album_sort: i32,
    pub track_sort: i32,
    pub albums_hires_only: bool,
    pub queue_open: bool,
    /// Most recent first.
    pub recent_searches: Vec<String>,
    /// Last known capabilities of each hw: device.
    pub dac_caps: crate::dac::CapsCache,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            window: None,
            page: page_name(Page::Home).into(),
            page_arg: String::new(),
            album_sort: 0,
            track_sort: 0,
            albums_hires_only: false,
            queue_open: false,
            recent_searches: Vec::new(),
            dac_caps: Default::default(),
        }
    }
}

pub fn default_path() -> PathBuf {
    ricercar_core::config::data_dir().join("ui-state.json")
}

impl UiState {
    /// Missing or unreadable file: defaults.
    pub fn load(path: &Path) -> UiState {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        ricercar_core::config::write_atomic(path, &json)
    }

    pub fn page(&self) -> Page {
        page_from_name(&self.page).unwrap_or(Page::Home)
    }
}

const PAGES: [(Page, &str); 13] = [
    (Page::Home, "home"),
    (Page::Albums, "albums"),
    (Page::Album, "album"),
    (Page::Artists, "artists"),
    (Page::Artist, "artist"),
    (Page::Tracks, "tracks"),
    (Page::Genres, "genres"),
    (Page::Genre, "genre"),
    (Page::Favorites, "favorites"),
    (Page::Playlist, "playlist"),
    (Page::Radio, "radio"),
    (Page::Search, "search"),
    (Page::Settings, "settings"),
];

pub fn page_name(p: Page) -> &'static str {
    PAGES
        .iter()
        .find(|(q, _)| *q == p)
        .map(|(_, n)| *n)
        .unwrap_or("home")
}

pub fn page_from_name(n: &str) -> Option<Page> {
    PAGES.iter().find(|(_, m)| *m == n).map(|(p, _)| *p)
}

/// The page to open at startup. The search page depends on text that is not
/// kept, and an album or playlist may have gone since: fall back to Home.
pub fn start_page(
    st: &UiState,
    album_exists: impl Fn(&str) -> bool,
    playlist_exists: impl Fn(i64) -> bool,
) -> (Page, String) {
    let page = st.page();
    let arg = st.page_arg.clone();
    let ok = match page {
        Page::Search => false,
        Page::Album => album_exists(&arg),
        Page::Playlist => arg.parse().is_ok_and(&playlist_exists),
        Page::Artist | Page::Genre => !arg.is_empty(),
        _ => true,
    };
    if ok {
        (page, arg)
    } else {
        (Page::Home, String::new())
    }
}

/// A saved size is only reused when it is plausible (not a collapsed or
/// absurd window from a broken session).
pub fn usable_size(w: &WindowState) -> Option<(f32, f32)> {
    let ok = |v: f32| v.is_finite() && (320.0..=16384.0).contains(&v);
    (ok(w.width) && ok(w.height)).then_some((w.width, w.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ui-state.json");
        assert_eq!(UiState::load(&p), UiState::default());
        let st = UiState {
            window: Some(WindowState {
                width: 1600.0,
                height: 1000.0,
                maximized: false,
                x: None,
                y: None,
            }),
            page: "artists".into(),
            album_sort: 1,
            track_sort: 2,
            albums_hires_only: true,
            queue_open: true,
            recent_searches: vec!["bach".into()],
            ..Default::default()
        };
        st.save(&p).unwrap();
        let back = UiState::load(&p);
        assert_eq!(back, st);
        assert_eq!(back.page(), Page::Artists);
        // Unknown or partial files fall back field by field.
        std::fs::write(&p, r#"{"page":"nowhere","track_sort":3}"#).unwrap();
        let st = UiState::load(&p);
        assert_eq!((st.page(), st.track_sort, st.window), (Page::Home, 3, None));
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(UiState::load(&p), UiState::default());
    }

    #[test]
    fn page_names_cover_every_page() {
        for (p, n) in PAGES {
            assert_eq!(page_from_name(n), Some(p));
            assert_eq!(page_name(p), n);
        }
    }

    #[test]
    fn start_page_falls_back_home() {
        let st = |page: &str, arg: &str| UiState {
            page: page.into(),
            page_arg: arg.into(),
            ..Default::default()
        };
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let pl = |id: i64| id == 7;
        assert_eq!(start_page(&st("artists", ""), no, pl).0, Page::Artists);
        assert_eq!(start_page(&st("album", "abc"), yes, pl).0, Page::Album);
        assert_eq!(start_page(&st("album", "abc"), no, pl).0, Page::Home);
        assert_eq!(start_page(&st("playlist", "7"), no, pl).0, Page::Playlist);
        assert_eq!(start_page(&st("playlist", "8"), no, pl).0, Page::Home);
        assert_eq!(start_page(&st("search", ""), yes, pl).0, Page::Home);
        assert_eq!(start_page(&st("artist", ""), yes, pl).0, Page::Home);
    }

    #[test]
    fn sizes_are_sanity_checked() {
        let w = |width, height| WindowState {
            width,
            height,
            maximized: false,
            x: None,
            y: None,
        };
        assert_eq!(usable_size(&w(1600.0, 1000.0)), Some((1600.0, 1000.0)));
        assert_eq!(usable_size(&w(0.0, 1000.0)), None);
        assert_eq!(usable_size(&w(f32::NAN, 900.0)), None);
    }
}
