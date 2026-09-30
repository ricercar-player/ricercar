//! Plugin items in context menus and pages (docs/plugins.md, Items):
//! "Go to album" / "Go to artist" through `album_ref` / `artist_ref` (a
//! search by name without them), the label of a plugin album, and the
//! plugin's own `actions`, listed below the host's entries.
//!
//! Lists turn plugin items into plain tracks and cards, so the items with
//! refs or actions are remembered here (`remember`) to be found again when
//! a menu opens on them.

use std::collections::HashMap;
use std::rc::Rc;

use ricercar_core::TrackInfo;
use ricercar_core::library::Track;
use ricercar_core::plugin::{ActionKind, Item, ItemKind, TRACKS_MAX, parse_plugin_uri};
use ricercar_core::{EnqueueAt, PlayContext};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app::{Ui, post};
use crate::plugins::{
    ALBUM_CARD, CARD, browse_arg, card_target, error_text, host, name_of, parse_arg,
};
use crate::text::t;
use crate::{ItemAction, Page, TrackMenu};

/// Items remembered at most; the memory starts over past it.
const KNOWN_MAX: usize = 20_000;

/// Remember the items of a list that carry refs or actions.
pub fn remember(ui: &Ui, id: &str, items: &[Item]) {
    remember_in(&mut ui.plugins.borrow_mut().known, id, items);
}

pub fn remember_in(known: &mut HashMap<(String, String), Item>, id: &str, items: &[Item]) {
    for it in items.iter().filter(|i| {
        !i.actions.is_empty()
            || i.album_ref.is_some()
            || i.artist_ref.is_some()
            || i.label_ref.is_some()
    }) {
        if known.len() >= KNOWN_MAX {
            known.clear();
        }
        known.insert((id.to_string(), it.reference.clone()), it.clone());
    }
}

pub fn find_item(ui: &Ui, id: &str, reference: &str) -> Option<Item> {
    ui.plugins.borrow().lookup(id, reference)
}

/// A plugin track's refs, from the track itself or its remembered item.
fn with_refs(ui: &Ui, mut info: TrackInfo) -> TrackInfo {
    if (info.album_ref.is_none() || info.artist_ref.is_none())
        && let Some(it) = parse_plugin_uri(&info.uri).and_then(|(id, r)| find_item(ui, &id, &r))
    {
        info.album_ref = info.album_ref.or(it.album_ref);
        info.artist_ref = info.artist_ref.or(it.artist_ref);
    }
    info
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.clone().filter(|s| !s.trim().is_empty())
}

// ------------------------------------------------------------ navigation

/// "Go to album" on a track of any source: its album page, a plugin's
/// `album_ref`, else a search of the plugin by name.
pub fn go_to_album(ui: &Rc<Ui>, info: &TrackInfo) {
    if let Some(aid) = non_empty(&info.album_id) {
        return ui.navigate(Page::Album, &aid, true);
    }
    let info = with_refs(ui, info.clone());
    if let Some((id, r)) = info.plugin_album_ref() {
        let title = non_empty(&info.album).unwrap_or_else(|| t("Album").into());
        let arg = format!("{ALBUM_CARD}{}", browse_arg(&id, &r, &title));
        return ui.navigate(Page::Album, &arg, true);
    }
    crate::plugins::open_track_album(ui, &info);
}

/// "Go to artist": the artist page (local files, plugins without
/// `artist_ref`), or the plugin's page of that artist.
pub fn go_to_artist(ui: &Rc<Ui>, info: &TrackInfo) {
    if info.path.is_some() {
        let name = info
            .album_artist
            .clone()
            .or(info.artist.clone())
            .unwrap_or_default();
        return ui.navigate(Page::Artist, &name, true);
    }
    let info = with_refs(ui, info.clone());
    if let Some((id, r)) = info.plugin_artist_ref() {
        let title = non_empty(&info.artist)
            .or(non_empty(&info.album_artist))
            .unwrap_or_else(|| t("Artist").into());
        return ui.navigate(Page::Browse, &browse_arg(&id, &r, &title), true);
    }
    crate::plugins::open_track_artist(ui, &info);
}

/// "Go to album" / "Go to artist" on a plugin track of a list.
pub fn track_nav(ui: &Rc<Ui>, tr: &Track, album: bool) {
    let info = TrackInfo::from(tr);
    if album {
        go_to_album(ui, &info);
    } else {
        go_to_artist(ui, &info);
    }
}

/// "Go to artist" on a plugin album card: its `artist_ref`, else the
/// artist page by name.
pub fn card_artist(ui: &Rc<Ui>, id: &str, reference: &str) {
    let Some(it) = find_item(ui, id, reference) else {
        return;
    };
    let name = non_empty(&it.artist).or(non_empty(&it.album_artist));
    match (&it.artist_ref, name) {
        (Some(r), name) => {
            let title = name.unwrap_or_else(|| t("Artist").into());
            ui.navigate(Page::Browse, &browse_arg(id, r, &title), true);
        }
        (None, Some(name)) => ui.navigate(Page::Artist, &name, true),
        (None, None) => {}
    }
}

/// Browse arguments of a plugin album's artist and label ("" when the
/// plugin gave no ref), for the links of the album page.
pub fn album_links(id: &str, it: &Item) -> (String, String) {
    let artist = it
        .artist_ref
        .as_ref()
        .map(|r| {
            let name = non_empty(&it.artist)
                .or(non_empty(&it.album_artist))
                .unwrap_or_else(|| t("Artist").into());
            browse_arg(id, r, &name)
        })
        .unwrap_or_default();
    let label = it
        .label_ref
        .as_ref()
        .map(|r| browse_arg(id, r, t("Label")))
        .unwrap_or_default();
    (artist, label)
}

/// A queue entry's menu: "Go to album", "Go to artist", "Remove".
pub fn queue_action(ui: &Rc<Ui>, entry: u64, action: &str) {
    let info = {
        let st = ui.ctx.ctl.lock();
        st.queue
            .iter()
            .find(|q| q.id == entry)
            .map(|q| q.info.clone())
    };
    let Some(info) = info else { return };
    match action {
        "album" | "artist" => {
            ui.app().set_now_playing_open(false);
            if action == "album" {
                go_to_album(ui, &info);
            } else {
                go_to_artist(ui, &info);
            }
        }
        "remove" => ui.ctx.ctl.remove_ids(&[entry]),
        _ => {}
    }
}

// ------------------------------------------------------------ menus

/// Where "Go to album" / "Go to artist" lead for a track.
fn track_nav_flags(info: &TrackInfo) -> (bool, bool) {
    let album = non_empty(&info.album_id).is_some()
        || non_empty(&info.album).is_some()
        || info.album_ref.is_some();
    let artist = non_empty(&info.artist).is_some()
        || non_empty(&info.album_artist).is_some()
        || info.artist_ref.is_some();
    (album, artist)
}

/// The plugin item under a menu, and where its navigation entries lead.
fn menu_target(ui: &Ui, is_album: bool) -> (Option<(String, Item)>, bool, bool) {
    let tm = ui.window.global::<TrackMenu>();
    if is_album {
        let aid = tm.get_album_id().to_string();
        let Some((id, reference, _)) = card_target(&aid).and_then(|(arg, _)| parse_arg(arg)) else {
            return (None, false, false);
        };
        let Some(it) = find_item(ui, &id, &reference) else {
            return (None, false, false);
        };
        let artist = it.kind == ItemKind::Album
            && (it.artist_ref.is_some()
                || non_empty(&it.artist).is_some()
                || non_empty(&it.album_artist).is_some());
        return (Some((id, it)), false, artist);
    }
    let list = tm.get_list().to_string();
    let index = tm.get_index().max(0);
    let info = if list == "queue" {
        let st = ui.ctx.ctl.lock();
        st.queue
            .iter()
            .find(|q| q.id == index as u64)
            .map(|q| q.info.clone())
    } else {
        crate::views::list_tracks(ui, &list)
            .get(index as usize)
            .map(TrackInfo::from)
    };
    let Some(info) = info.map(|i| with_refs(ui, i)) else {
        return (None, false, false);
    };
    let (album, artist) = track_nav_flags(&info);
    let item =
        parse_plugin_uri(&info.uri).and_then(|(id, r)| find_item(ui, &id, &r).map(|it| (id, it)));
    (item, album, artist)
}

/// A context menu opened: fill its plugin part.
pub fn menu_opened(ui: &Ui, is_album: bool) {
    let (target, album, artist) = menu_target(ui, is_album);
    let actions: Vec<ItemAction> = target
        .as_ref()
        .map(|(_, it)| {
            it.actions
                .iter()
                .map(|a| ItemAction {
                    label: a.label.clone().into(),
                    play: a.kind == ActionKind::Play,
                })
                .collect()
        })
        .unwrap_or_default();
    ui.plugins.borrow_mut().menu = target;
    let app = ui.app();
    app.set_menu_actions(ModelRc::new(VecModel::from(actions)));
    app.set_menu_album_nav(album);
    app.set_menu_artist_nav(artist);
    app.set_action_busy(-1);
}

/// Run an action of the item under the menu. "browse" opens its ref;
/// "play" reads the tracks off the UI thread, then replaces the queue or,
/// with `append`, adds to it.
pub fn run_action(ui: &Rc<Ui>, index: usize, append: bool) {
    let Some((id, item)) = ui.plugins.borrow().menu.clone() else {
        return;
    };
    let Some(action) = item.actions.get(index).cloned() else {
        return;
    };
    if action.kind == ActionKind::Browse {
        let arg = browse_arg(&id, &action.reference, &action.label);
        return ui.navigate(Page::Browse, &arg, true);
    }
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.action_serial += 1;
        pv.action_serial
    };
    ui.app().set_action_busy(index as i32);
    let h = host(ui);
    std::thread::spawn(move || {
        let r = h.playable_tracks(&id, &action.reference, TRACKS_MAX);
        post(move |ui| {
            if ui.plugins.borrow().action_serial == serial {
                ui.app().set_action_busy(-1);
            }
            match r {
                Ok(infos) if infos.is_empty() => ui.toast(t("Nothing playable here"), true),
                Ok(infos) if append => {
                    ui.ctx.ctl.enqueue(infos, EnqueueAt::End);
                    ui.toast(t("Added to the queue"), false);
                }
                Ok(infos) => ui.ctx.ctl.play_tracks(infos, 0, PlayContext::None),
                Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
            }
        });
    });
}

/// The browse page opened on a plugin item: offer its menu when the item
/// has actions (asked with `item.get` when it was not seen in a list).
pub fn browse_opened(ui: &Ui, id: &str, reference: &str, serial: u64) {
    fn menu_id(id: &str, it: &Item) -> String {
        if it.actions.is_empty() {
            return String::new();
        }
        format!("{CARD}{}", browse_arg(id, &it.reference, &it.title))
    }
    if let Some(it) = find_item(ui, id, reference) {
        ui.app().set_br_menu_id(menu_id(id, &it).into());
        return;
    }
    ui.app().set_br_menu_id("".into());
    let (h, id, reference) = (host(ui), id.to_string(), reference.to_string());
    std::thread::spawn(move || {
        let Ok(it) = h.item_get(&id, &reference) else {
            return;
        };
        post(move |ui| {
            remember(ui, &id, std::slice::from_ref(&it));
            if ui.plugins.borrow().browse_serial == serial {
                ui.app().set_br_menu_id(menu_id(&id, &it).into());
            }
        });
    });
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_menu_opened(|is_album| crate::app::with_ui(|ui| menu_opened(ui, is_album)));
    app.on_item_action(|i, append| {
        crate::app::with_ui(|ui| run_action(ui, i.max(0) as usize, append))
    });
}
