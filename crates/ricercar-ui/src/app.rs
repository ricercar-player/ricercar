//! UI-thread state shared by every view, and the entry point.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use ricercar_core::library::{Album, Artist, Track};
use ricercar_daemon::AppContext;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::images::{Loader, Lookup, Source};
use crate::text::t;
use crate::ui_state::{UiState, WindowState};
use crate::{AlbumCard, App, ArtistCard, MainWindow, Page, TrackRow};

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

/// Run `f` on the UI (from the UI thread only).
pub fn with_ui(f: impl FnOnce(&Rc<Ui>)) {
    let ui = UI.with(|u| u.borrow().clone());
    if let Some(ui) = ui {
        f(&ui);
    }
}

/// Run `f` on the UI from any thread.
pub fn post(f: impl FnOnce(&Rc<Ui>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_ui(f));
}

pub const TILE: u32 = 320;
pub const THUMB: u32 = 96;
pub const LARGE: u32 = 640;

/// A list model plus two indexes, cover key → rows and track path → rows,
/// so arriving covers and the "playing" / favourite markers only touch the
/// rows concerned instead of walking every list.
pub struct Rows<T: Row> {
    pub model: Rc<VecModel<T>>,
    by_cover: RefCell<HashMap<SharedString, Vec<usize>>>,
    by_path: RefCell<HashMap<SharedString, Vec<usize>>>,
}

pub trait Row: Clone + 'static {
    fn ckey(&self) -> &SharedString;
    /// Track rows: the file path.
    fn path(&self) -> Option<&SharedString> {
        None
    }
    fn cover_missing(&self) -> bool;
    fn set_cover(&mut self, img: slint::Image);
}

macro_rules! cover_row {
    ($t:ty) => {
        impl Row for $t {
            fn ckey(&self) -> &SharedString {
                &self.ckey
            }
            fn cover_missing(&self) -> bool {
                !self.ckey.is_empty() && self.cover.size().width == 0
            }
            fn set_cover(&mut self, img: slint::Image) {
                self.cover = img;
            }
        }
    };
}
cover_row!(AlbumCard);
cover_row!(ArtistCard);
cover_row!(crate::QueueRow);

impl Row for TrackRow {
    fn ckey(&self) -> &SharedString {
        &self.ckey
    }
    fn path(&self) -> Option<&SharedString> {
        Some(&self.key)
    }
    fn cover_missing(&self) -> bool {
        !self.ckey.is_empty() && self.cover.size().width == 0
    }
    fn set_cover(&mut self, img: slint::Image) {
        self.cover = img;
    }
}

impl<T: Row> Default for Rows<T> {
    fn default() -> Self {
        Rows {
            model: Rc::new(VecModel::default()),
            by_cover: RefCell::default(),
            by_path: RefCell::default(),
        }
    }
}

impl<T: Row> std::ops::Deref for Rows<T> {
    type Target = VecModel<T>;
    fn deref(&self) -> &VecModel<T> {
        &self.model
    }
}

impl<T: Row> Rows<T> {
    fn index(&self, rows: &[T], offset: usize) {
        let mut covers = self.by_cover.borrow_mut();
        let mut paths = self.by_path.borrow_mut();
        for (i, r) in rows.iter().enumerate() {
            if !r.ckey().is_empty() {
                covers.entry(r.ckey().clone()).or_default().push(offset + i);
            }
            if let Some(p) = r.path() {
                paths.entry(p.clone()).or_default().push(offset + i);
            }
        }
    }

    pub fn set(&self, rows: Vec<T>) {
        self.by_cover.borrow_mut().clear();
        self.by_path.borrow_mut().clear();
        self.index(&rows, 0);
        self.model.set_vec(rows);
    }

    /// Append rows (lists loaded in chunks).
    pub fn extend(&self, rows: Vec<T>) {
        self.index(&rows, self.model.row_count());
        self.model.extend(rows);
    }

    /// Put a freshly decoded cover on the rows that wait for it.
    pub fn patch_cover(&self, key: &str, img: &slint::Image) {
        let Some(idx) = self.by_cover.borrow().get(key).cloned() else {
            return;
        };
        for i in idx {
            if let Some(mut r) = self.model.row_data(i)
                && r.cover_missing()
            {
                r.set_cover(img.clone());
                self.model.set_row_data(i, r);
            }
        }
    }

    /// Update the rows of one track; `f` returns false to skip the write.
    pub fn update_path(&self, path: &str, f: impl Fn(&mut T) -> bool) {
        let Some(idx) = self.by_path.borrow().get(path).cloned() else {
            return;
        };
        for i in idx {
            if let Some(mut r) = self.model.row_data(i)
                && f(&mut r)
            {
                self.model.set_row_data(i, r);
            }
        }
    }
}

/// All the list models the window shows, kept so covers and "playing"
/// markers can be patched in place.
#[derive(Default)]
pub struct Models {
    pub albums: Rows<AlbumCard>,
    pub home_played: Rows<AlbumCard>,
    pub home_added: Rows<AlbumCard>,
    pub home_top: Rows<AlbumCard>,
    pub al_more: Rows<AlbumCard>,
    pub ar_own: Rows<AlbumCard>,
    pub ar_appears: Rows<AlbumCard>,
    pub ge_albums: Rows<AlbumCard>,
    pub fav_albums: Rows<AlbumCard>,
    pub s_albums: Rows<AlbumCard>,
    pub artists: Rows<ArtistCard>,
    pub s_artists: Rows<ArtistCard>,
    pub tracks: Rows<TrackRow>,
    pub home_tracks: Rows<TrackRow>,
    pub al_tracks: Rows<TrackRow>,
    pub ar_top: Rows<TrackRow>,
    pub fav_tracks: Rows<TrackRow>,
    pub pl_tracks: Rows<TrackRow>,
    pub s_tracks: Rows<TrackRow>,
    pub queue: Rows<crate::QueueRow>,
    /// Plugin browse page and plugin search tab.
    pub br_cards: Rows<AlbumCard>,
    pub br_tracks: Rows<TrackRow>,
}

impl Models {
    fn album_models(&self) -> [&Rows<AlbumCard>; 11] {
        [
            &self.br_cards,
            &self.albums,
            &self.home_played,
            &self.home_added,
            &self.home_top,
            &self.al_more,
            &self.ar_own,
            &self.ar_appears,
            &self.ge_albums,
            &self.fav_albums,
            &self.s_albums,
        ]
    }

    pub fn track_models(&self) -> [&Rows<TrackRow>; 8] {
        [
            &self.br_tracks,
            &self.tracks,
            &self.home_tracks,
            &self.al_tracks,
            &self.ar_top,
            &self.fav_tracks,
            &self.pl_tracks,
            &self.s_tracks,
        ]
    }

    /// The model behind a track list name used in callbacks.
    pub fn track_model(&self, list: &str) -> Option<&Rows<TrackRow>> {
        Some(match list {
            "tracks" => &self.tracks,
            "home-tracks" => &self.home_tracks,
            "album" => &self.al_tracks,
            "artist-top" => &self.ar_top,
            "fav" => &self.fav_tracks,
            "playlist" => &self.pl_tracks,
            "search" => &self.s_tracks,
            "browse" => &self.br_tracks,
            _ => return None,
        })
    }
}

#[derive(Default)]
pub struct State {
    pub history: Vec<(Page, String)>,
    pub hist_pos: usize,
    /// Tracks behind each track list, for activation and menus.
    pub lists: HashMap<String, Vec<Track>>,
    /// Albums behind album models, by album id.
    pub albums: HashMap<String, Album>,
    pub artists: Vec<Artist>,
    /// Cover sources by cache key.
    pub sources: HashMap<String, (Source, Option<Lookup>)>,
    pub lib_rev: u64,
    pub queue_rev: u64,
    pub now_key: Option<String>,
    pub now_path: Option<String>,
    /// Covers decoded since the last refill: (key, size).
    pub arrived: Vec<(String, u32)>,
    /// Track path currently marked as playing in the lists.
    pub marked_playing: Option<String>,
    pub search_serial: u64,
    /// Bumped on every Tracks page load, to drop stale background chunks.
    pub tracks_serial: u64,
    /// Raw colour picked from the playing cover (made readable per theme).
    pub cover_rgb: Option<[u8; 3]>,
}

pub struct Ui {
    pub window: MainWindow,
    pub ctx: Rc<AppContext>,
    pub st: RefCell<State>,
    pub models: Models,
    pub loader: RefCell<Loader>,
    pub timers: RefCell<Vec<slint::Timer>>,
    pub extras: RefCell<crate::extras::Extras>,
    pub player: RefCell<crate::player::PlayerView>,
    pub tray: RefCell<Option<ksni::blocking::Handle<crate::tray::Tray>>>,
    pub visible: std::cell::Cell<bool>,
    /// Interface state as last saved to ui-state.json.
    pub saved_state: RefCell<UiState>,
    pub plugins: RefCell<crate::plugins::PluginsView>,
}

impl Ui {
    pub fn app(&self) -> App<'_> {
        self.window.global::<App>()
    }

    pub fn toast(&self, msg: impl Into<SharedString>, error: bool) {
        let app = self.app();
        app.set_toast(msg.into());
        app.set_toast_error(error);
        app.set_toast_serial(app.get_toast_serial() + 1);
    }

    /// Register where a cover key comes from and return it if already decoded.
    pub fn cover(&self, key: &str, src: Source, lookup: Option<Lookup>, size: u32) -> slint::Image {
        self.st
            .borrow_mut()
            .sources
            .insert(key.to_string(), (src.clone(), lookup.clone()));
        self.loader
            .borrow_mut()
            .get(&src, size, lookup)
            .unwrap_or_default()
    }

    /// Register a cover source without loading it yet (large lists).
    pub fn cover_lazy(
        &self,
        key: &str,
        src: Source,
        lookup: Option<Lookup>,
        size: u32,
    ) -> slint::Image {
        self.st
            .borrow_mut()
            .sources
            .insert(key.to_string(), (src, lookup));
        self.loader.borrow().peek(key, size).unwrap_or_default()
    }

    pub fn request_cover(&self, key: &str, size: u32) {
        let src = self.st.borrow().sources.get(key).cloned();
        if let Some((src, lookup)) = src {
            let _ = self.loader.borrow_mut().get(&src, size, lookup);
        }
    }

    /// Patch freshly decoded covers into the rows waiting for them
    /// (debounced: covers arrive in bursts).
    pub fn refill_covers(&self) {
        let arrived = std::mem::take(&mut self.st.borrow_mut().arrived);
        if arrived.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let loader = self.loader.borrow();
        for (key, size) in &arrived {
            let Some(img) = loader.peek(key, *size) else {
                continue;
            };
            match *size {
                TILE => {
                    for m in self.models.album_models() {
                        m.patch_cover(key, &img);
                    }
                    self.models.artists.patch_cover(key, &img);
                    self.models.s_artists.patch_cover(key, &img);
                }
                THUMB => {
                    for m in self.models.track_models() {
                        m.patch_cover(key, &img);
                    }
                    self.models.queue.patch_cover(key, &img);
                }
                _ => {}
            }
        }
        drop(loader);
        crate::extras::refill_station_covers(self);
        crate::views::refill_page_covers(self);
        crate::player::refill_now_cover(self);
        crate::profile::add("refill covers", started.elapsed());
    }

    // ------------------------------------------------------------ navigation

    pub fn navigate(self: &Rc<Self>, page: Page, arg: &str, push: bool) {
        if push {
            let mut st = self.st.borrow_mut();
            let pos = st.hist_pos;
            if st.history.get(pos).map(|(p, a)| (*p, a.as_str())) != Some((page, arg)) {
                st.history.truncate(pos + 1);
                st.history.push((page, arg.to_string()));
                st.hist_pos = st.history.len() - 1;
            }
        }
        self.loader.borrow_mut().cancel_pending();
        {
            let _p = crate::profile::span(format!("page {page:?}: load"));
            crate::views::load(self, page, arg);
        }
        crate::profile::expect_frame(format!("page {page:?}"));
        let app = self.app();
        app.set_page(page);
        let st = self.st.borrow();
        app.set_can_back(st.hist_pos > 0);
        app.set_can_forward(st.hist_pos + 1 < st.history.len());
    }

    pub fn back(self: &Rc<Self>) {
        let target = {
            let mut st = self.st.borrow_mut();
            if st.hist_pos == 0 {
                return;
            }
            st.hist_pos -= 1;
            st.history[st.hist_pos].clone()
        };
        self.navigate(target.0, &target.1, false);
    }

    pub fn forward(self: &Rc<Self>) {
        let target = {
            let mut st = self.st.borrow_mut();
            if st.hist_pos + 1 >= st.history.len() {
                return;
            }
            st.hist_pos += 1;
            st.history[st.hist_pos].clone()
        };
        self.navigate(target.0, &target.1, false);
    }

    /// Re-run the loader of the current page (library changed).
    pub fn reload_page(self: &Rc<Self>) {
        let cur = {
            let st = self.st.borrow();
            st.history.get(st.hist_pos).cloned()
        };
        if let Some((p, a)) = cur {
            crate::views::load(self, p, &a);
        }
    }
}

/// Show or hide the main window (tray icon, MPRIS Raise).
pub fn toggle_window(ui: &Rc<Ui>) {
    if ui.visible.get() {
        let _ = ui.window.hide();
        ui.visible.set(false);
    } else {
        let _ = ui.window.show();
        ui.visible.set(true);
    }
    sync_tray(ui);
}

/// Push playback state and title to the tray menu.
pub fn sync_tray(ui: &Ui) {
    let Some(h) = ui.tray.borrow().as_ref().cloned() else {
        return;
    };
    let app = ui.app();
    let playing = app.get_playing();
    let title = if app.get_has_track() {
        format!("{} — {}", app.get_title(), app.get_artist())
    } else {
        String::new()
    };
    let visible = ui.visible.get();
    // ksni's blocking update round-trips to its D-Bus thread: do it off the UI thread.
    std::thread::spawn(move || {
        h.update(|t| {
            t.playing = playing;
            t.title = title;
            t.visible = visible;
        });
    });
}

pub fn model<T: Row>(m: &Rows<T>) -> ModelRc<T> {
    ModelRc::from(m.model.clone())
}

pub fn run(args: ricercar_daemon::Args) -> Result<(), Box<dyn std::error::Error>> {
    let lang = crate::text::detect_language();
    let _ = slint::select_bundled_translation(lang);

    let hooks = ricercar_daemon::Hooks {
        on_raise: Some(Arc::new(|| {
            post(|ui| {
                let _ = ui.window.show();
                ui.visible.set(true);
                sync_tray(ui);
            });
        })),
        on_quit: Some(Arc::new(|| {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        })),
    };
    let snapshot = std::env::var_os("RICERCAR_SNAPSHOT").map(std::path::PathBuf::from);
    if snapshot.is_some() {
        crate::snapshot::install();
    }
    let ctx = Rc::new(ricercar_daemon::startup(args, hooks)?);
    let window = MainWindow::new()?;
    // Wayland app-id / X11 class matching ricercar.desktop (icon, grouping);
    // needs the backend (created above) and must precede `show()`.
    let _ = slint::set_xdg_app_id("ricercar");

    let loader = Loader::new(
        ctx.covers.clone(),
        Box::new(|id, buf| {
            post(move |ui| {
                let key = crate::images::split_cache_id(&id);
                if buf.is_some()
                    && let Some(k) = key
                {
                    ui.st.borrow_mut().arrived.push(k);
                }
                ui.loader.borrow_mut().arrived(id, buf);
            })
        }),
    );
    loader.set_online(ctx.config.read().unwrap().online.cover_art);

    let ui = Rc::new(Ui {
        window: window.clone_strong(),
        ctx: ctx.clone(),
        st: RefCell::new(State::default()),
        models: Models::default(),
        loader: RefCell::new(loader),
        timers: RefCell::new(Vec::new()),
        extras: RefCell::new(crate::extras::Extras::default()),
        player: RefCell::new(crate::player::PlayerView::default()),
        tray: RefCell::new(None),
        visible: std::cell::Cell::new(true),
        saved_state: RefCell::new(UiState::default()),
        plugins: RefCell::new(crate::plugins::PluginsView::default()),
    });
    UI.with(|u| *u.borrow_mut() = Some(ui.clone()));

    bind_models(&ui);
    crate::views::wire(&ui);
    crate::player::wire(&ui);
    crate::extras::wire(&ui);
    crate::plugins::wire(&ui);
    crate::updates::wire(&ui);
    let snapshot_mode = snapshot.is_some();
    // The screenshot and perf tours start from a known state; the state
    // tour checks that it survives a restart.
    let keep_state = !snapshot_mode || crate::snapshot::tour() == "state";
    let (page, arg) = if !keep_state {
        (Page::Home, String::new())
    } else {
        restore_state(&ui)
    };
    ui.navigate(page, &arg, true);
    crate::updates::check(&ui);
    if let Some(dir) = snapshot {
        crate::snapshot::start(&ui, dir);
    }

    // Covers arrive in bursts; patch models at most every 60 ms.
    let t = slint::Timer::default();
    t.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(60),
        || with_ui(|ui| ui.refill_covers()),
    );
    ui.timers.borrow_mut().push(t);

    if keep_state {
        // No resize event in Slint: look for changes every 2 s, so a crash
        // or a kill loses at most that much.
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(2),
            || with_ui(|ui| save_state(ui)),
        );
        ui.timers.borrow_mut().push(t);
    }

    let tray_enabled = !snapshot_mode && ctx.config.read().unwrap().ui.tray;
    if tray_enabled {
        *ui.tray.borrow_mut() = crate::tray::spawn();
    }
    window.window().on_close_requested(|| {
        let mut keep = false;
        with_ui(|ui| {
            let cfg = ui.ctx.config.read().unwrap().ui.clone();
            keep = cfg.close_to_tray && ui.tray.borrow().is_some();
            if keep {
                ui.visible.set(false);
                crate::app::sync_tray(ui);
            }
        });
        if keep {
            slint::CloseRequestResponse::HideWindow
        } else {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
    });
    if crate::profile::enabled() && !snapshot_mode {
        // Not available with every renderer; the headless tour hooks its own.
        let _ = window.window().set_rendering_notifier(|state, _| {
            if matches!(state, slint::RenderingState::AfterRendering) {
                crate::profile::frame_rendered();
            }
        });
    }
    window.show()?;
    slint::run_event_loop_until_quit()?;
    if keep_state {
        save_state(&ui);
    }
    let _ = window.hide();
    ctx.shutdown();
    UI.with(|u| *u.borrow_mut() = None);
    Ok(())
}

fn bind_models(ui: &Rc<Ui>) {
    let app = ui.app();
    let m = &ui.models;
    app.set_albums(model(&m.albums));
    app.set_home_played(model(&m.home_played));
    app.set_home_added(model(&m.home_added));
    app.set_home_top(model(&m.home_top));
    app.set_home_tracks(model(&m.home_tracks));
    app.set_al_more(model(&m.al_more));
    app.set_al_tracks(model(&m.al_tracks));
    app.set_ar_own(model(&m.ar_own));
    app.set_ar_appears(model(&m.ar_appears));
    app.set_ar_top(model(&m.ar_top));
    app.set_ge_albums(model(&m.ge_albums));
    app.set_fav_albums(model(&m.fav_albums));
    app.set_fav_tracks(model(&m.fav_tracks));
    app.set_s_albums(model(&m.s_albums));
    app.set_s_artists(model(&m.s_artists));
    app.set_s_tracks(model(&m.s_tracks));
    app.set_artists(model(&m.artists));
    app.set_tracks(model(&m.tracks));
    app.set_pl_tracks(model(&m.pl_tracks));
    app.set_queue(model(&m.queue));
    app.set_br_cards(model(&m.br_cards));
    app.set_br_tracks(model(&m.br_tracks));
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.set_greeting(crate::text::greeting().into());
}

// ------------------------------------------------------------ row builders

pub fn album_card(ui: &Ui, a: &Album, eager: bool) -> AlbumCard {
    let src = Source::Track {
        key: a.id.clone(),
        path: a.cover_path.clone().into(),
    };
    let lookup = (a.artist != "Various Artists").then(|| Lookup {
        artist: a.artist.clone(),
        album: a.title.clone(),
    });
    let cover = if eager {
        ui.cover(&a.id, src, lookup, TILE)
    } else {
        ui.cover_lazy(&a.id, src, lookup, TILE)
    };
    ui.st.borrow_mut().albums.insert(a.id.clone(), a.clone());
    AlbumCard {
        id: a.id.clone().into(),
        title: a.title.clone().into(),
        artist: a.artist.clone().into(),
        year: a.year.map(|y| y.to_string()).unwrap_or_default().into(),
        cover,
        ckey: a.id.clone().into(),
        hires: a.is_hires(),
        fav: a.favorite,
        plugin: false,
        source: Default::default(),
    }
}

pub fn artist_card(ui: &Ui, a: &Artist, eager: bool) -> ArtistCard {
    let key = format!("p:{}", a.cover_path);
    let src = Source::Track {
        key: key.clone(),
        path: a.cover_path.clone().into(),
    };
    let cover = if eager {
        ui.cover(&key, src, None, TILE)
    } else {
        ui.cover_lazy(&key, src, None, TILE)
    };
    ArtistCard {
        name: a.name.clone().into(),
        albums: a.album_count as i32,
        tracks: a.track_count as i32,
        cover,
        ckey: key.into(),
        source: Default::default(),
    }
}

pub struct RowOpts {
    pub cover: bool,
    pub eager: bool,
    /// Number rows by track number (album) instead of position.
    pub track_numbers: bool,
    pub discs: bool,
    /// Hide the artist when it equals the album artist.
    pub album_artist: Option<String>,
    /// Position of the first row in the list (chunked lists).
    pub start: usize,
}

pub fn track_rows(ui: &Ui, tracks: &[Track], o: RowOpts) -> Vec<TrackRow> {
    let now = ui.st.borrow().now_path.clone();
    // Plugin names, looked up once per list (and only when needed).
    let names = if tracks.iter().any(Track::is_plugin) {
        crate::plugins::names(ui)
    } else {
        Default::default()
    };
    let multi_disc = o.discs
        && tracks
            .iter()
            .filter_map(|t| t.disc)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1;
    let mut last_disc = None;
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let disc = if multi_disc && t.disc != last_disc {
                last_disc = t.disc;
                format!("{} {}", crate::text::t("Disc"), t.disc.unwrap_or(1))
            } else {
                String::new()
            };
            let (cover, ckey) = if o.cover && t.is_plugin() {
                // Plugin tracks: their art URL, through the cover cache.
                match t.art.clone().filter(|a| a.starts_with("http")) {
                    Some(url) => {
                        let src = Source::Url(url.clone());
                        let img = if o.eager {
                            ui.cover(&url, src, None, THUMB)
                        } else {
                            ui.cover_lazy(&url, src, None, THUMB)
                        };
                        (img, url)
                    }
                    None => (slint::Image::default(), String::new()),
                }
            } else if o.cover {
                let src = Source::Track {
                    key: t.album_id.clone(),
                    path: t.path.clone().into(),
                };
                let img = if o.eager {
                    ui.cover(&t.album_id, src, None, THUMB)
                } else {
                    ui.cover_lazy(&t.album_id, src, None, THUMB)
                };
                (img, t.album_id.clone())
            } else {
                (slint::Image::default(), String::new())
            };
            let artist = match (&o.album_artist, &t.artist) {
                (Some(aa), Some(a)) if aa.eq_ignore_ascii_case(a) => String::new(),
                (_, a) => a.clone().unwrap_or_default(),
            };
            TrackRow {
                key: t.path.clone().into(),
                n: if o.track_numbers {
                    t.track.map(|n| n.to_string()).unwrap_or_default()
                } else {
                    (o.start + i + 1).to_string()
                }
                .into(),
                title: t.title.clone().into(),
                artist: artist.into(),
                album: t.album.clone().unwrap_or_default().into(),
                album_id: t.album_id.clone().into(),
                dur: crate::text::mmss(t.duration_ms).into(),
                fav: t.favorite,
                playing: now.as_deref() == Some(t.path.as_str()),
                hires: t.is_hires(),
                fmt: {
                    let q =
                        crate::text::quality(t.sample_rate, t.bits, t.codec.as_deref(), t.bitrate);
                    match t.codec.as_deref() {
                        Some(c) if !q.is_empty() => format!("{c} {q}"),
                        Some(c) => c.to_string(),
                        None => q,
                    }
                }
                .into(),
                disc: disc.into(),
                cover,
                ckey: ckey.into(),
                plays: t.play_count as i32,
                source: ricercar_core::plugin::parse_plugin_uri(&t.path)
                    .map(|(id, _)| names.get(&id).cloned().unwrap_or(id))
                    .unwrap_or_default()
                    .into(),
            }
        })
        .collect()
}

pub fn set_rows<T: Row>(m: &Rows<T>, rows: Vec<T>) {
    m.set(rows);
}

pub fn unknown_artist() -> &'static str {
    t("Unknown artist")
}

// ------------------------------------------------------------ window state

/// X11 reports and accepts window positions; Wayland does neither.
pub fn is_x11() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some()
}

/// Apply the saved interface state (before the window is shown) and return
/// the page to open.
fn restore_state(ui: &Rc<Ui>) -> (Page, String) {
    let st = UiState::load(&crate::ui_state::default_path());
    let app = ui.app();
    app.set_album_sort(st.album_sort);
    app.set_track_sort(st.track_sort);
    app.set_albums_hires_only(st.albums_hires_only);
    app.set_queue_open(st.queue_open);
    let win = ui.window.window();
    if let Some(w) = &st.window {
        if let Some((width, height)) = crate::ui_state::usable_size(w) {
            win.set_size(slint::LogicalSize::new(width, height));
        }
        if let (Some(x), Some(y), true) = (w.x, w.y, is_x11()) {
            win.set_position(slint::PhysicalPosition::new(x, y));
        }
        if w.maximized {
            win.set_maximized(true);
        }
    }
    let lib = &ui.ctx.lib;
    let start = crate::ui_state::start_page(
        &st,
        |id| lib.album(id).is_some(),
        |id| lib.playlist(id).is_some(),
    );
    *ui.saved_state.borrow_mut() = st;
    start
}

pub fn current_state(ui: &Ui) -> UiState {
    let app = ui.app();
    let win = ui.window.window();
    let saved = ui.saved_state.borrow().clone();
    let maximized = win.is_maximized();
    let size = win.size().to_logical(win.scale_factor());
    // A maximized window keeps the size it will return to.
    let (width, height) = match (&saved.window, maximized) {
        (Some(w), true) => (w.width, w.height),
        _ => (size.width, size.height),
    };
    let pos = is_x11().then(|| win.position());
    let (page, arg) = {
        let st = ui.st.borrow();
        st.history
            .get(st.hist_pos)
            .cloned()
            .unwrap_or((Page::Home, String::new()))
    };
    UiState {
        window: (width > 0.0 && height > 0.0).then_some(WindowState {
            width,
            height,
            maximized,
            x: pos.map(|p| p.x),
            y: pos.map(|p| p.y),
        }),
        page: crate::ui_state::page_name(page).into(),
        page_arg: arg,
        album_sort: app.get_album_sort(),
        track_sort: app.get_track_sort(),
        albums_hires_only: app.get_albums_hires_only(),
        queue_open: app.get_queue_open(),
        recent_searches: saved.recent_searches,
        dac_caps: saved.dac_caps,
    }
}

/// Write ui-state.json when something changed.
pub fn save_state(ui: &Ui) {
    let st = current_state(ui);
    if st == *ui.saved_state.borrow() {
        return;
    }
    match st.save(&crate::ui_state::default_path()) {
        Ok(()) => *ui.saved_state.borrow_mut() = st,
        Err(e) => tracing::warn!("save ui-state.json: {e}"),
    }
}
