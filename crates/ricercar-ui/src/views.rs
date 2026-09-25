//! Page loaders and list/album/playlist actions.

use std::rc::Rc;

use ricercar_core::library::{AlbumSort, Track, TrackSort};
use ricercar_core::{EnqueueAt, PlayContext, TrackInfo};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::app::{
    RowOpts, THUMB, TILE, Ui, album_card, artist_card, set_rows, track_rows, with_ui,
};
use crate::images::{Lookup, Source};
use crate::text::{long_duration, quality, t};
use crate::{GenreCard, Page, PlaylistItem};

const GENRE_TINTS: &[u32] = &[
    0x8c4a3b, 0x3b5f8c, 0x6b3b8c, 0x3b8c6b, 0x8c7a3b, 0x8c3b62, 0x2f6f7a, 0x5a6b2f, 0x7a4a2f,
    0x3b4a8c,
];

fn tint(name: &str) -> slint::Color {
    let h = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let c = GENRE_TINTS[(h as usize) % GENRE_TINTS.len()];
    slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn album_sort(i: i32) -> AlbumSort {
    match i {
        1 => AlbumSort::Artist,
        2 => AlbumSort::YearDesc,
        3 => AlbumSort::RecentlyAdded,
        _ => AlbumSort::Title,
    }
}

fn track_sort(i: i32) -> TrackSort {
    match i {
        1 => TrackSort::Title,
        2 => TrackSort::Album,
        3 => TrackSort::Duration,
        4 => TrackSort::RecentlyAdded,
        5 => TrackSort::MostPlayed,
        _ => TrackSort::Artist,
    }
}

fn opts(cover: bool) -> RowOpts {
    RowOpts {
        cover,
        eager: true,
        track_numbers: false,
        discs: false,
        album_artist: None,
    }
}

pub fn load(ui: &Rc<Ui>, page: Page, arg: &str) {
    let lib = ui.ctx.lib.clone();
    let app = ui.app();
    let m = &ui.models;
    match page {
        Page::Home => {
            app.set_greeting(crate::text::greeting().into());
            let played: Vec<_> = lib
                .recently_played_albums(14)
                .iter()
                .map(|a| album_card(ui, a, true))
                .collect();
            set_rows(&m.home_played, played);
            let added: Vec<_> = lib
                .recently_added_albums(14)
                .iter()
                .map(|a| album_card(ui, a, true))
                .collect();
            set_rows(&m.home_added, added);
            let top: Vec<_> = lib
                .most_played_albums(14)
                .iter()
                .map(|a| album_card(ui, a, true))
                .collect();
            set_rows(&m.home_top, top);
            let tracks = lib.most_played_tracks(6);
            set_rows(&m.home_tracks, track_rows(ui, &tracks, opts(true)));
            ui.st
                .borrow_mut()
                .lists
                .insert("home-tracks".into(), tracks);
            refresh_stats(ui);
        }
        Page::Albums => load_albums(ui),
        Page::Album => load_album(ui, arg),
        Page::Artists => {
            let artists = lib.artists();
            let rows: Vec<_> = artists.iter().map(|a| artist_card(ui, a, false)).collect();
            set_rows(&m.artists, rows);
            ui.st.borrow_mut().artists = artists;
            visible(ui, "artists", 0, 40);
        }
        Page::Artist => load_artist(ui, arg),
        Page::Tracks => {
            let tracks = lib.tracks(track_sort(app.get_track_sort()));
            let o = RowOpts {
                eager: false,
                ..opts(true)
            };
            set_rows(&m.tracks, track_rows(ui, &tracks, o));
            ui.st.borrow_mut().lists.insert("tracks".into(), tracks);
            visible(ui, "tracks", 0, 30);
        }
        Page::Genres => {
            let rows: Vec<_> = lib
                .genres()
                .into_iter()
                .map(|g| GenreCard {
                    tint: tint(&g.name),
                    name: g.name.into(),
                    albums: g.album_count as i32,
                })
                .collect();
            app.set_genres(ModelRc::new(VecModel::from(rows)));
        }
        Page::Genre => {
            app.set_ge_name(arg.into());
            let rows: Vec<_> = lib
                .genre_albums(arg)
                .iter()
                .map(|a| album_card(ui, a, true))
                .collect();
            set_rows(&m.ge_albums, rows);
        }
        Page::Favorites => {
            let albums: Vec<_> = lib
                .favorite_albums()
                .iter()
                .map(|a| album_card(ui, a, true))
                .collect();
            set_rows(&m.fav_albums, albums);
            let tracks = lib.favorite_tracks();
            set_rows(&m.fav_tracks, track_rows(ui, &tracks, opts(true)));
            ui.st.borrow_mut().lists.insert("fav".into(), tracks);
        }
        Page::Playlist => load_playlist(ui, arg.parse().unwrap_or(0)),
        Page::Search => run_search(ui, &app.get_search_text()),
        Page::Radio => crate::extras::load_radio(ui),
        Page::Settings => crate::extras::load_settings(ui),
    }
}

pub fn load_albums(ui: &Rc<Ui>) {
    let app = ui.app();
    let hires_only = app.get_albums_hires_only();
    let albums: Vec<_> = ui
        .ctx
        .lib
        .albums(album_sort(app.get_album_sort()))
        .into_iter()
        .filter(|a| !hires_only || a.is_hires())
        .collect();
    let rows: Vec<_> = albums.iter().map(|a| album_card(ui, a, false)).collect();
    set_rows(&ui.models.albums, rows);
    visible(ui, "albums", 0, 36);
}

fn load_album(ui: &Rc<Ui>, id: &str) {
    let lib = &ui.ctx.lib;
    let Some(a) = lib.album(id) else { return };
    let app = ui.app();
    let tracks = lib.album_tracks(id);
    app.set_al_id(a.id.clone().into());
    app.set_al_title(a.title.clone().into());
    app.set_al_artist(a.artist.clone().into());
    app.set_al_year(a.year.map(|y| y.to_string()).unwrap_or_default().into());
    app.set_al_genre(a.genre.clone().unwrap_or_default().into());
    app.set_al_count(a.track_count as i32);
    app.set_al_duration(long_duration(a.duration_ms).into());
    let q = quality(a.max_rate, a.max_bits, a.codec.as_deref(), None);
    app.set_al_quality(
        match (&a.codec, q.is_empty()) {
            (Some(c), false) => format!("{c} {q}"),
            (Some(c), true) => c.clone(),
            (None, _) => q,
        }
        .into(),
    );
    app.set_al_hires(a.is_hires());
    app.set_al_fav(a.favorite);
    app.set_al_path(a.dir.clone().into());
    let src = Source::Track {
        key: a.id.clone(),
        path: a.cover_path.clone().into(),
    };
    let lookup = Some(Lookup {
        artist: a.artist.clone(),
        album: a.title.clone(),
    });
    app.set_al_cover(ui.cover(&a.id, src, lookup, crate::app::LARGE));
    let o = RowOpts {
        cover: false,
        eager: true,
        track_numbers: true,
        discs: true,
        album_artist: Some(a.artist.clone()),
    };
    set_rows(&ui.models.al_tracks, track_rows(ui, &tracks, o));
    ui.st.borrow_mut().lists.insert("album".into(), tracks);
    let (own, _) = lib.artist_albums(&a.artist);
    let more: Vec<_> = own
        .iter()
        .filter(|x| x.id != a.id)
        .take(12)
        .map(|x| album_card(ui, x, true))
        .collect();
    set_rows(&ui.models.al_more, more);
}

fn load_artist(ui: &Rc<Ui>, name: &str) {
    let lib = &ui.ctx.lib;
    let app = ui.app();
    let (own, appears) = lib.artist_albums(name);
    let top = lib.artist_top_tracks(name, 5);
    app.set_ar_name(name.into());
    app.set_ar_albums((own.len()) as i32);
    app.set_ar_tracks(own.iter().map(|a| a.track_count as i32).sum());
    let cover = own.first().or(appears.first()).map(|a| {
        ui.cover(
            &a.id,
            Source::Track {
                key: a.id.clone(),
                path: a.cover_path.clone().into(),
            },
            None,
            crate::app::LARGE,
        )
    });
    app.set_ar_cover(cover.unwrap_or_default());
    set_rows(&ui.models.ar_top, track_rows(ui, &top, opts(true)));
    ui.st.borrow_mut().lists.insert("artist-top".into(), top);
    let own_rows: Vec<_> = own.iter().map(|a| album_card(ui, a, true)).collect();
    set_rows(&ui.models.ar_own, own_rows);
    let app_rows: Vec<_> = appears.iter().map(|a| album_card(ui, a, true)).collect();
    set_rows(&ui.models.ar_appears, app_rows);
}

fn load_playlist(ui: &Rc<Ui>, id: i64) {
    let lib = &ui.ctx.lib;
    let app = ui.app();
    let Some(p) = lib.playlist(id) else { return };
    let tracks = lib.playlist_tracks(id);
    app.set_pl_id(id as i32);
    app.set_pl_name(p.name.into());
    app.set_pl_count(tracks.len() as i32);
    app.set_pl_duration(long_duration(tracks.iter().map(|t| t.duration_ms).sum()).into());
    let cover = tracks.first().map(|t| {
        ui.cover(
            &t.album_id,
            Source::Track {
                key: t.album_id.clone(),
                path: t.path.clone().into(),
            },
            None,
            crate::app::LARGE,
        )
    });
    app.set_pl_cover(cover.unwrap_or_default());
    set_rows(&ui.models.pl_tracks, track_rows(ui, &tracks, opts(true)));
    ui.st.borrow_mut().lists.insert("playlist".into(), tracks);
}

pub fn refresh_playlists(ui: &Rc<Ui>) {
    let rows: Vec<_> = ui
        .ctx
        .lib
        .playlists()
        .into_iter()
        .map(|p| PlaylistItem {
            id: p.id as i32,
            name: p.name.into(),
            tracks: p.track_count as i32,
            cover: Default::default(),
            ckey: Default::default(),
        })
        .collect();
    ui.app().set_playlists(ModelRc::new(VecModel::from(rows)));
}

pub fn refresh_stats(ui: &Rc<Ui>) {
    let s = ui.ctx.lib.stats();
    let app = ui.app();
    app.set_lib_tracks(s.tracks as i32);
    app.set_lib_albums(s.albums as i32);
    app.set_lib_artists(s.artists as i32);
    app.set_lib_hires(s.hires_tracks as i32);
    app.set_lib_duration(long_duration(s.duration_ms).into());
}

fn run_search(ui: &Rc<Ui>, q: &str) {
    let r = ui.ctx.lib.search(q);
    let m = &ui.models;
    let artists: Vec<_> = r.artists.iter().map(|a| artist_card(ui, a, true)).collect();
    set_rows(&m.s_artists, artists);
    let albums: Vec<_> = r.albums.iter().map(|a| album_card(ui, a, true)).collect();
    set_rows(&m.s_albums, albums);
    let tracks: Vec<Track> = r.tracks.into_iter().take(50).collect();
    set_rows(&m.s_tracks, track_rows(ui, &tracks, opts(true)));
    ui.st.borrow_mut().lists.insert("search".into(), tracks);
}

/// Load covers for the rows on screen (virtualized pages).
pub fn visible(ui: &Rc<Ui>, list: &str, first: usize, last: usize) {
    let keys: Vec<(String, u32)> = match list {
        "albums" => {
            let m = &ui.models.albums;
            (first..last.min(m.row_count()))
                .filter_map(|i| m.row_data(i))
                .filter(|r| r.cover.size().width == 0)
                .map(|r| (r.ckey.to_string(), TILE))
                .collect()
        }
        "artists" => {
            let m = &ui.models.artists;
            (first..last.min(m.row_count()))
                .filter_map(|i| m.row_data(i))
                .filter(|r| r.cover.size().width == 0)
                .map(|r| (r.ckey.to_string(), TILE))
                .collect()
        }
        other => match ui.models.track_model(other) {
            Some(m) => (first..last.min(m.row_count()))
                .filter_map(|i| m.row_data(i))
                .filter(|r| r.cover.size().width == 0 && !r.ckey.is_empty())
                .map(|r| (r.ckey.to_string(), THUMB))
                .collect(),
            None => Vec::new(),
        },
    };
    if keys.is_empty() {
        return;
    }
    ui.loader.borrow_mut().cancel_pending();
    for (k, size) in keys {
        ui.request_cover(&k, size);
    }
}

pub fn refill_page_covers(ui: &Ui) {
    let app = ui.app();
    let loader = ui.loader.borrow();
    let large = crate::app::LARGE;
    if app.get_al_cover().size().width == 0
        && let Some(i) = loader.peek(&app.get_al_id(), large)
    {
        app.set_al_cover(i);
    }
    if app.get_ar_cover().size().width == 0
        && let Some(first) = ui
            .models
            .ar_own
            .row_data(0)
            .or(ui.models.ar_appears.row_data(0))
        && let Some(i) = loader.peek(&first.ckey, large)
    {
        app.set_ar_cover(i);
    }
    if app.get_pl_cover().size().width == 0
        && let Some(first) = ui.models.pl_tracks.row_data(0)
        && let Some(i) = loader.peek(&first.album_id, large)
    {
        app.set_pl_cover(i);
    }
}

// ------------------------------------------------------------------ actions

fn infos(tracks: &[Track]) -> Vec<TrackInfo> {
    tracks.iter().map(TrackInfo::from).collect()
}

fn list_tracks(ui: &Ui, list: &str) -> Vec<Track> {
    ui.st.borrow().lists.get(list).cloned().unwrap_or_default()
}

fn context_for(ui: &Ui, list: &str) -> PlayContext {
    let app = ui.app();
    match list {
        "album" => PlayContext::Album(app.get_al_id().to_string()),
        "artist-top" => PlayContext::Artist(app.get_ar_name().to_string()),
        "playlist" => PlayContext::Playlist(app.get_pl_id() as i64),
        _ => PlayContext::None,
    }
}

pub fn track_activate(ui: &Rc<Ui>, list: &str, index: usize) {
    let tracks = list_tracks(ui, list);
    if tracks.is_empty() {
        return;
    }
    let ctx = context_for(ui, list);
    // Huge lists (all tracks, search): queue a window after the pick.
    let (slice, start) = if tracks.len() > 500 {
        let end = (index + 500).min(tracks.len());
        (&tracks[index..end], 0)
    } else {
        (&tracks[..], index)
    };
    ui.ctx.ctl.play_library_tracks(slice, start, ctx);
}

pub fn track_action(ui: &Rc<Ui>, list: &str, index: usize, action: &str) {
    let tracks = list_tracks(ui, list);
    let Some(tr) = tracks.get(index).cloned() else {
        return;
    };
    let ctl = &ui.ctx.ctl;
    match action {
        "play" => track_activate(ui, list, index),
        "next" => {
            ctl.enqueue(vec![TrackInfo::from(&tr)], EnqueueAt::Next);
            ui.toast(t("Will play next"), false);
        }
        "queue" => {
            ctl.enqueue(vec![TrackInfo::from(&tr)], EnqueueAt::End);
            ui.toast(t("Added to the queue"), false);
        }
        "album" => ui.navigate(Page::Album, &tr.album_id, true),
        "artist" => {
            let name = tr
                .album_artist
                .clone()
                .or(tr.artist.clone())
                .unwrap_or_default();
            ui.navigate(Page::Artist, &name, true);
        }
        "fav" => {
            let fav = !ui.ctx.lib.is_favorite(&tr.path);
            ui.ctx.lib.set_favorite(&tr.path, fav);
            mark_fav(ui, &tr.path, fav);
            ui.toast(
                if fav {
                    t("Added to favorites")
                } else {
                    t("Removed from favorites")
                },
                false,
            );
        }
        "folder" => show_in_folder(&tr.path),
        "pl-remove" => {
            let id = ui.app().get_pl_id() as i64;
            ui.ctx.lib.remove_from_playlist(id, &[index]);
            load_playlist(ui, id);
            refresh_playlists(ui);
            ui.toast(t("Removed from the playlist"), false);
        }
        a if a.starts_with("pl:") => add_to_playlist(ui, &a[3..], &[tr.path.clone()]),
        _ => {}
    }
}

fn add_to_playlist(ui: &Rc<Ui>, target: &str, paths: &[String]) {
    let lib = &ui.ctx.lib;
    let id = if target == "new" {
        // The dialog just created it: the most recent playlist.
        lib.playlists()
            .into_iter()
            .max_by_key(|p| p.id)
            .map(|p| p.id)
    } else {
        target.parse().ok()
    };
    let Some(id) = id else { return };
    lib.add_to_playlist(id, paths);
    refresh_playlists(ui);
    let name = lib.playlist(id).map(|p| p.name).unwrap_or_default();
    ui.toast(format!("+ {name}"), false);
}

fn mark_fav(ui: &Ui, path: &str, fav: bool) {
    for m in ui.models.track_models() {
        for i in 0..m.row_count() {
            let mut r = m.row_data(i).unwrap();
            if r.key == path {
                r.fav = fav;
                m.set_row_data(i, r);
            }
        }
    }
    crate::player::sync_fav(ui);
}

pub fn show_in_folder(path: &str) {
    let dir = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    // Prefer the FileManager1 portal (selects the file), fall back to xdg-open.
    let uri = ricercar_core::meta::file_uri(std::path::Path::new(path));
    let ok = zbus::blocking::Connection::session()
        .and_then(|c| {
            c.call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri.as_str()], ""),
            )
        })
        .is_ok();
    if !ok {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

pub fn album_action(ui: &Rc<Ui>, id: &str, action: &str) {
    let lib = ui.ctx.lib.clone();
    let ctl = &ui.ctx.ctl;
    match action {
        "artist-play" | "artist-shuffle" => {
            let (own, appears) = lib.artist_albums(id);
            let mut tracks: Vec<Track> = own.iter().flat_map(|a| lib.album_tracks(&a.id)).collect();
            if tracks.is_empty() {
                tracks = appears
                    .iter()
                    .flat_map(|a| lib.album_tracks(&a.id))
                    .collect();
            }
            let ctx = PlayContext::Artist(id.to_string());
            if action == "artist-shuffle" {
                ctl.play_shuffled(infos(&tracks), ctx);
            } else {
                ctl.play_tracks(infos(&tracks), 0, ctx);
            }
            return;
        }
        "genre-shuffle" => {
            let tracks: Vec<Track> = lib
                .genre_albums(id)
                .iter()
                .flat_map(|a| lib.album_tracks(&a.id))
                .collect();
            ctl.play_shuffled(infos(&tracks), PlayContext::Mix);
            return;
        }
        _ => {}
    }
    let tracks = lib.album_tracks(id);
    if tracks.is_empty() {
        return;
    }
    let ctx = PlayContext::Album(id.to_string());
    match action {
        "play" => ctl.play_tracks(infos(&tracks), 0, ctx),
        "shuffle" => ctl.play_shuffled(infos(&tracks), ctx),
        "next" => {
            ctl.enqueue(infos(&tracks), EnqueueAt::Next);
            ui.toast(t("Will play next"), false);
        }
        "queue" => {
            ctl.enqueue(infos(&tracks), EnqueueAt::End);
            ui.toast(t("Added to the queue"), false);
        }
        "artist" => {
            let name = lib.album(id).map(|a| a.artist).unwrap_or_default();
            ui.navigate(Page::Artist, &name, true);
        }
        "fav" => {
            let fav = !lib.album(id).map(|a| a.favorite).unwrap_or(false);
            lib.set_album_favorite(id, fav);
            if ui.app().get_al_id() == id {
                ui.app().set_al_fav(fav);
            }
            ui.toast(
                if fav {
                    t("Added to favorites")
                } else {
                    t("Removed from favorites")
                },
                false,
            );
        }
        "folder" => show_in_folder(&tracks[0].path),
        a if a.starts_with("pl:") => {
            let paths: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
            add_to_playlist(ui, &a[3..], &paths);
        }
        _ => {}
    }
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_navigate(|page, arg| with_ui(|ui| ui.navigate(page, &arg, true)));
    app.on_back(|| with_ui(|ui| ui.back()));
    app.on_forward(|| with_ui(|ui| ui.forward()));
    app.on_album_activate(|id| with_ui(|ui| ui.navigate(Page::Album, &id, true)));
    app.on_artist_activate(|name| with_ui(|ui| ui.navigate(Page::Artist, &name, true)));
    app.on_track_activate(|list, i| with_ui(|ui| track_activate(ui, &list, i.max(0) as usize)));
    app.on_track_action(|list, i, a| with_ui(|ui| track_action(ui, &list, i.max(0) as usize, &a)));
    app.on_album_action(|id, a| with_ui(|ui| album_action(ui, &id, &a)));
    app.on_visible(|list, first, last| {
        with_ui(|ui| visible(ui, &list, first.max(0) as usize, last.max(0) as usize))
    });
    app.on_albums_sort_changed(|| with_ui(load_albums));
    app.on_tracks_sort_changed(|| with_ui(|ui| load(ui, Page::Tracks, "")));
    app.on_shuffle_all(|| {
        with_ui(|ui| {
            let tracks = ui.ctx.lib.random_tracks(300);
            ui.ctx.ctl.play_tracks(infos(&tracks), 0, PlayContext::Mix);
        })
    });
    app.on_search(|q: SharedString| {
        with_ui(|ui| {
            let q = q.trim().to_string();
            if q.is_empty() {
                if ui.app().get_page() == Page::Search {
                    ui.back();
                }
                return;
            }
            // Debounce: search after typing pauses for 150 ms.
            let serial = {
                let mut st = ui.st.borrow_mut();
                st.search_serial += 1;
                st.search_serial
            };
            slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
                with_ui(|ui| {
                    if ui.st.borrow().search_serial != serial {
                        return;
                    }
                    if ui.app().get_page() == Page::Search {
                        run_search(ui, &q);
                    } else {
                        ui.navigate(Page::Search, "", true);
                    }
                })
            });
        })
    });

    // playlists
    app.on_playlist_create(|name| {
        with_ui(|ui| {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            ui.ctx.lib.create_playlist(name);
            refresh_playlists(ui);
            ui.toast(t("Playlist created"), false);
        })
    });
    app.on_playlist_rename(|id, name| {
        with_ui(|ui| {
            ui.ctx.lib.rename_playlist(id as i64, name.trim());
            refresh_playlists(ui);
            load_playlist(ui, id as i64);
        })
    });
    app.on_playlist_delete(|id| {
        with_ui(|ui| {
            ui.ctx.lib.delete_playlist(id as i64);
            refresh_playlists(ui);
            ui.toast(t("Playlist deleted"), false);
            ui.navigate(Page::Home, "", true);
        })
    });
    app.on_playlist_play(|id, shuffle| {
        with_ui(|ui| {
            let tracks = ui.ctx.lib.playlist_tracks(id as i64);
            let ctx = PlayContext::Playlist(id as i64);
            if shuffle {
                ui.ctx.ctl.play_shuffled(infos(&tracks), ctx);
            } else {
                ui.ctx.ctl.play_tracks(infos(&tracks), 0, ctx);
            }
        })
    });
    app.on_playlist_export(|id| {
        with_ui(|ui| {
            let name = ui
                .ctx
                .lib
                .playlist(id as i64)
                .map(|p| p.name)
                .unwrap_or_default();
            let dir = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join("Music")
                .join("Playlists");
            let _ = std::fs::create_dir_all(&dir);
            let safe: String = name
                .chars()
                .map(|c| if c == '/' { '-' } else { c })
                .collect();
            let dest = dir.join(format!("{safe}.m3u8"));
            match ui.ctx.lib.export_m3u(id as i64, &dest) {
                Ok(()) => ui.toast(
                    format!("{} {}", t("Playlist exported to"), dest.display()),
                    false,
                ),
                Err(e) => ui.toast(e.to_string(), true),
            }
        })
    });
    app.on_playlist_import(|| {
        std::thread::spawn(|| {
            let file = rfd::FileDialog::new()
                .add_filter("M3U", &["m3u", "m3u8"])
                .pick_file();
            if let Some(f) = file {
                crate::app::post(move |ui| match ui.ctx.lib.import_m3u(&f) {
                    Ok(id) => {
                        refresh_playlists(ui);
                        ui.toast(t("Playlist imported"), false);
                        ui.navigate(Page::Playlist, &id.to_string(), true);
                    }
                    Err(e) => ui.toast(format!("{}: {e}", t("Could not open the file")), true),
                });
            }
        });
    });
    app.on_show_in_folder(|p| show_in_folder(&p));
    app.on_open_url(|u| {
        let _ = std::process::Command::new("xdg-open")
            .arg(u.as_str())
            .spawn();
    });
    app.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    refresh_playlists(ui);
    app.set_track_menu(ModelRc::default());
}
