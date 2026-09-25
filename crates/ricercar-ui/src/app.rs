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

/// All the list models the window shows, kept so covers and "playing"
/// markers can be patched in place.
pub struct Models {
    pub albums: Rc<VecModel<AlbumCard>>,
    pub home_played: Rc<VecModel<AlbumCard>>,
    pub home_added: Rc<VecModel<AlbumCard>>,
    pub home_top: Rc<VecModel<AlbumCard>>,
    pub al_more: Rc<VecModel<AlbumCard>>,
    pub ar_own: Rc<VecModel<AlbumCard>>,
    pub ar_appears: Rc<VecModel<AlbumCard>>,
    pub ge_albums: Rc<VecModel<AlbumCard>>,
    pub fav_albums: Rc<VecModel<AlbumCard>>,
    pub s_albums: Rc<VecModel<AlbumCard>>,
    pub artists: Rc<VecModel<ArtistCard>>,
    pub s_artists: Rc<VecModel<ArtistCard>>,
    pub tracks: Rc<VecModel<TrackRow>>,
    pub home_tracks: Rc<VecModel<TrackRow>>,
    pub al_tracks: Rc<VecModel<TrackRow>>,
    pub ar_top: Rc<VecModel<TrackRow>>,
    pub fav_tracks: Rc<VecModel<TrackRow>>,
    pub pl_tracks: Rc<VecModel<TrackRow>>,
    pub s_tracks: Rc<VecModel<TrackRow>>,
}

impl Models {
    fn new() -> Models {
        Models {
            albums: Rc::new(VecModel::default()),
            home_played: Rc::new(VecModel::default()),
            home_added: Rc::new(VecModel::default()),
            home_top: Rc::new(VecModel::default()),
            al_more: Rc::new(VecModel::default()),
            ar_own: Rc::new(VecModel::default()),
            ar_appears: Rc::new(VecModel::default()),
            ge_albums: Rc::new(VecModel::default()),
            fav_albums: Rc::new(VecModel::default()),
            s_albums: Rc::new(VecModel::default()),
            artists: Rc::new(VecModel::default()),
            s_artists: Rc::new(VecModel::default()),
            tracks: Rc::new(VecModel::default()),
            home_tracks: Rc::new(VecModel::default()),
            al_tracks: Rc::new(VecModel::default()),
            ar_top: Rc::new(VecModel::default()),
            fav_tracks: Rc::new(VecModel::default()),
            pl_tracks: Rc::new(VecModel::default()),
            s_tracks: Rc::new(VecModel::default()),
        }
    }

    fn album_models(&self) -> [&Rc<VecModel<AlbumCard>>; 10] {
        [
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

    pub fn track_models(&self) -> [&Rc<VecModel<TrackRow>>; 7] {
        [
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
    pub fn track_model(&self, list: &str) -> Option<&Rc<VecModel<TrackRow>>> {
        Some(match list {
            "tracks" => &self.tracks,
            "home-tracks" => &self.home_tracks,
            "album" => &self.al_tracks,
            "artist-top" => &self.ar_top,
            "fav" => &self.fav_tracks,
            "playlist" => &self.pl_tracks,
            "search" => &self.s_tracks,
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
    pub covers_dirty: bool,
    pub search_serial: u64,
}

pub struct Ui {
    pub window: MainWindow,
    pub ctx: Rc<AppContext>,
    pub st: RefCell<State>,
    pub models: Models,
    pub loader: RefCell<Loader>,
    pub queue_model: Rc<VecModel<crate::QueueRow>>,
    pub timers: RefCell<Vec<slint::Timer>>,
    pub extras: RefCell<crate::extras::Extras>,
    pub player: RefCell<crate::player::PlayerView>,
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

    /// Patch freshly decoded covers into every model (debounced).
    pub fn refill_covers(&self) {
        if !std::mem::take(&mut self.st.borrow_mut().covers_dirty) {
            return;
        }
        let loader = self.loader.borrow();
        for m in self.models.album_models() {
            for i in 0..m.row_count() {
                let mut r = m.row_data(i).unwrap();
                if r.cover.size().width == 0
                    && let Some(img) = loader.peek(&r.ckey, TILE)
                {
                    r.cover = img;
                    m.set_row_data(i, r);
                }
            }
        }
        for m in [&self.models.artists, &self.models.s_artists] {
            for i in 0..m.row_count() {
                let mut r = m.row_data(i).unwrap();
                if r.cover.size().width == 0
                    && let Some(img) = loader.peek(&r.ckey, TILE)
                {
                    r.cover = img;
                    m.set_row_data(i, r);
                }
            }
        }
        for m in self.models.track_models() {
            for i in 0..m.row_count() {
                let mut r = m.row_data(i).unwrap();
                if !r.ckey.is_empty()
                    && r.cover.size().width == 0
                    && let Some(img) = loader.peek(&r.ckey, THUMB)
                {
                    r.cover = img;
                    m.set_row_data(i, r);
                }
            }
        }
        let q = &self.queue_model;
        for i in 0..q.row_count() {
            let mut r = q.row_data(i).unwrap();
            if !r.ckey.is_empty()
                && r.cover.size().width == 0
                && let Some(img) = loader.peek(&r.ckey, THUMB)
            {
                r.cover = img;
                q.set_row_data(i, r);
            }
        }
        drop(loader);
        crate::extras::refill_station_covers(self);
        crate::views::refill_page_covers(self);
        crate::player::refill_now_cover(self);
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
        crate::views::load(self, page, arg);
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

pub fn model<T: Clone + 'static>(m: &Rc<VecModel<T>>) -> ModelRc<T> {
    ModelRc::from(m.clone())
}

pub fn run(args: ricercar_daemon::Args) -> Result<(), Box<dyn std::error::Error>> {
    let lang = crate::text::detect_language();
    let _ = slint::select_bundled_translation(lang);

    let hooks = ricercar_daemon::Hooks {
        on_raise: Some(Arc::new(|| {
            post(|ui| {
                let _ = ui.window.show();
            });
        })),
        on_quit: Some(Arc::new(|| {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        })),
    };
    let ctx = Rc::new(ricercar_daemon::startup(args, hooks)?);
    let window = MainWindow::new()?;

    let loader = Loader::new(
        ctx.covers.clone(),
        Box::new(|id, buf| {
            post(move |ui| {
                ui.loader.borrow_mut().arrived(id, buf);
                ui.st.borrow_mut().covers_dirty = true;
            })
        }),
    );
    loader.set_online(ctx.config.read().unwrap().online.cover_art);

    let ui = Rc::new(Ui {
        window: window.clone_strong(),
        ctx: ctx.clone(),
        st: RefCell::new(State::default()),
        models: Models::new(),
        loader: RefCell::new(loader),
        queue_model: Rc::new(VecModel::default()),
        timers: RefCell::new(Vec::new()),
        extras: RefCell::new(crate::extras::Extras::default()),
        player: RefCell::new(crate::player::PlayerView::default()),
    });
    UI.with(|u| *u.borrow_mut() = Some(ui.clone()));

    bind_models(&ui);
    crate::views::wire(&ui);
    crate::player::wire(&ui);
    crate::extras::wire(&ui);
    ui.navigate(Page::Home, "", true);

    // Covers arrive in bursts; patch models at most every 60 ms.
    let t = slint::Timer::default();
    t.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(60),
        || with_ui(|ui| ui.refill_covers()),
    );
    ui.timers.borrow_mut().push(t);

    window.run()?;
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
    app.set_queue(model(&ui.queue_model));
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
}

pub fn track_rows(ui: &Ui, tracks: &[Track], o: RowOpts) -> Vec<TrackRow> {
    let now = ui.st.borrow().now_path.clone();
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
            let (cover, ckey) = if o.cover {
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
                    (i + 1).to_string()
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
                fmt: crate::text::quality(t.sample_rate, t.bits, t.codec.as_deref(), t.bitrate)
                    .into(),
                disc: disc.into(),
                cover,
                ckey: ckey.into(),
                plays: t.play_count as i32,
            }
        })
        .collect()
}

pub fn set_rows<T: Clone + 'static>(m: &Rc<VecModel<T>>, rows: Vec<T>) {
    m.set_vec(rows);
}

pub fn unknown_artist() -> &'static str {
    t("Unknown artist")
}
