//! What plugins add to the artist and album pages (`item.details`:
//! biography, related shelves, facts) and radios started from a plugin
//! track, album or artist (`radio.next`). Both are asked off the UI thread,
//! after the page shows; answers for a page left since are dropped.

use std::rc::Rc;
use std::time::Duration;

use ricercar_core::TrackInfo;
use ricercar_core::library::Track;
use ricercar_core::plugin::{
    Item, ItemDetails, ItemKind, PluginStatus, RunState, Shelf, parse_plugin_uri, plugin_uri,
};
use slint::{ModelRc, VecModel};

use crate::app::{RowOpts, Rows, THUMB, TILE, Ui, model, post, track_rows, with_ui};
use crate::images::Source;
use crate::text::t;
use crate::{AlbumCard, ArtistCard, DetailShelf, TrackRow};

/// Track lists of the related shelves: `<prefix>-<shelf index>`.
const ALBUM_LISTS: &str = "album-related";
const ARTIST_LISTS: &str = "artist-related";

/// One related shelf, split by kind.
struct ShelfRows {
    title: String,
    list: String,
    cards: Rows<AlbumCard>,
    artists: Rows<ArtistCard>,
    tracks: Rows<TrackRow>,
}

#[derive(Default)]
pub struct DetailsView {
    /// Bumped by every album or artist page load.
    album_serial: u64,
    artist_serial: u64,
    album: Vec<ShelfRows>,
    artist: Vec<ShelfRows>,
    /// Artist page: details already asked of a plugin.
    artist_asked: bool,
    /// Artist page: the `plugin://` URI a radio starts from.
    artist_seed: Option<String>,
    /// The radio being started.
    radio_serial: u64,
}

fn status(ui: &Ui, id: &str) -> Option<PluginStatus> {
    ui.ctx
        .plugins
        .status(id)
        .filter(|s| s.state == RunState::Running)
}

// ------------------------------------------------------------ pages

/// A new album page (local or plugin): forget the previous details.
pub fn clear_album(ui: &Ui) {
    {
        let mut pv = ui.plugins.borrow_mut();
        pv.details.album_serial += 1;
        pv.details.album.clear();
    }
    ui.st
        .borrow_mut()
        .lists
        .retain(|k, _| !k.starts_with(ALBUM_LISTS));
    let app = ui.app();
    app.set_al_facts("".into());
    app.set_al_related(ModelRc::default());
}

/// A new artist page: forget the previous details and radio.
pub fn clear_artist(ui: &Ui) {
    {
        let mut pv = ui.plugins.borrow_mut();
        let d = &mut pv.details;
        d.artist_serial += 1;
        d.artist.clear();
        d.artist_asked = false;
        d.artist_seed = None;
    }
    ui.st
        .borrow_mut()
        .lists
        .retain(|k, _| !k.starts_with(ARTIST_LISTS));
    let app = ui.app();
    app.set_ar_bio("".into());
    app.set_ar_bio_source("".into());
    app.set_ar_facts("".into());
    app.set_ar_related(ModelRc::default());
    app.set_ar_radio(false);
}

/// A plugin album on the album page (`card_id`): ask its details.
pub fn load_album(ui: &Ui, card_id: &str, id: &str, reference: &str) {
    if !status(ui, id).is_some_and(|s| s.caps.details) {
        return;
    }
    let serial = ui.plugins.borrow().details.album_serial;
    let (h, card_id, id, reference) = (
        ui.ctx.plugins.clone(),
        card_id.to_string(),
        id.to_string(),
        reference.to_string(),
    );
    std::thread::spawn(move || {
        let r = h.item_details(&id, &reference);
        post(move |ui| {
            if ui.plugins.borrow().details.album_serial != serial
                || ui.app().get_al_id() != card_id.as_str()
            {
                return;
            }
            match r {
                Ok(d) => show_album(ui, &id, d),
                Err(e) => tracing::info!("plugin[{id}] details of {reference}: {e}"),
            }
        });
    });
}

fn show_album(ui: &Ui, id: &str, d: ItemDetails) {
    let app = ui.app();
    app.set_al_facts(facts_line(&d).into());
    let rows = shelves(ui, id, ALBUM_LISTS, &d.related);
    app.set_al_related(shelf_model(&rows));
    ui.plugins.borrow_mut().details.album = rows;
}

/// The artist page found `name` in plugin `id` as `reference`: the first
/// such plugin offering a radio gives the page its "Start radio", the first
/// with details its biography and shelves.
pub fn artist_found(ui: &Ui, id: &str, reference: &str, name: &str) {
    let Some(st) = status(ui, id) else { return };
    let (radio, ask, serial) = {
        let mut pv = ui.plugins.borrow_mut();
        let d = &mut pv.details;
        let radio = st.caps.radio && d.artist_seed.is_none();
        if radio {
            d.artist_seed = Some(plugin_uri(id, reference));
        }
        let ask = st.caps.details && !d.artist_asked;
        d.artist_asked |= ask;
        (radio, ask, d.artist_serial)
    };
    if radio {
        ui.app().set_ar_radio(true);
    }
    if !ask {
        return;
    }
    let (h, id, reference, name) = (
        ui.ctx.plugins.clone(),
        id.to_string(),
        reference.to_string(),
        name.to_string(),
    );
    std::thread::spawn(move || {
        let r = h.item_details(&id, &reference);
        post(move |ui| {
            if ui.plugins.borrow().details.artist_serial != serial
                || ui.app().get_ar_name() != name.as_str()
            {
                return;
            }
            match r {
                Ok(d) => show_artist(ui, &id, d),
                Err(e) => {
                    tracing::info!("plugin[{id}] details of {reference}: {e}");
                    // Another plugin that knows the artist may answer.
                    ui.plugins.borrow_mut().details.artist_asked = false;
                }
            }
        });
    });
}

fn show_artist(ui: &Ui, id: &str, d: ItemDetails) {
    let app = ui.app();
    if let Some(b) = &d.biography {
        app.set_ar_bio(b.text.clone().into());
        app.set_ar_bio_source(b.source.clone().unwrap_or_default().into());
    }
    app.set_ar_facts(facts_line(&d).into());
    let rows = shelves(ui, id, ARTIST_LISTS, &d.related);
    app.set_ar_related(shelf_model(&rows));
    ui.plugins.borrow_mut().details.artist = rows;
}

/// "Label: Demo Records · Released: 2024".
fn facts_line(d: &ItemDetails) -> String {
    d.facts
        .iter()
        .map(|f| format!("{}: {}", f.label, f.value))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn shelves(ui: &Ui, id: &str, prefix: &str, related: &[Shelf]) -> Vec<ShelfRows> {
    let pname = ui
        .ctx
        .plugins
        .status(id)
        .map(|s| s.name)
        .unwrap_or_else(|| id.to_string());
    related
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let list = format!("{prefix}-{i}");
            let cards = Rows::default();
            cards.set(
                s.items
                    .iter()
                    .filter(|it| !matches!(it.kind, ItemKind::Track | ItemKind::Artist))
                    .filter(|it| it.is_browsable())
                    .map(|it| crate::plugins::album_card(ui, id, &pname, it, true))
                    .collect(),
            );
            let artists = Rows::default();
            artists.set(
                s.items
                    .iter()
                    .filter(|it| it.kind == ItemKind::Artist)
                    .map(|it| artist_card(ui, &pname, it))
                    .collect(),
            );
            let tracks: Vec<Track> = s
                .items
                .iter()
                .filter(|it| it.kind == ItemKind::Track && it.is_playable())
                .map(|it| Track::from_info(&it.to_track_info(id)))
                .collect();
            let o = RowOpts {
                cover: true,
                eager: true,
                track_numbers: false,
                discs: false,
                album_artist: None,
                start: 0,
            };
            let track_rows_ = Rows::default();
            track_rows_.set(track_rows(ui, &tracks, o));
            ui.st.borrow_mut().lists.insert(list.clone(), tracks);
            ShelfRows {
                title: s.title.clone(),
                list,
                cards,
                artists,
                tracks: track_rows_,
            }
        })
        .collect()
}

fn artist_card(ui: &Ui, pname: &str, it: &Item) -> ArtistCard {
    let (cover, ckey) = match crate::plugins::art_url(it) {
        Some(url) => (ui.cover(&url, Source::Url(url.clone()), None, TILE), url),
        None => (slint::Image::default(), String::new()),
    };
    ArtistCard {
        name: it.title.clone().into(),
        albums: 0,
        tracks: 0,
        cover,
        ckey: ckey.into(),
        source: pname.into(),
    }
}

fn shelf_model(rows: &[ShelfRows]) -> ModelRc<DetailShelf> {
    let shelves: Vec<DetailShelf> = rows
        .iter()
        .map(|s| DetailShelf {
            title: s.title.clone().into(),
            list: s.list.clone().into(),
            cards: model(&s.cards),
            artists: model(&s.artists),
            tracks: model(&s.tracks),
        })
        .collect();
    ModelRc::new(VecModel::from(shelves))
}

/// A decoded cover for the rows of the related shelves waiting for it.
pub fn patch_cover(ui: &Ui, key: &str, size: u32, img: &slint::Image) {
    let pv = ui.plugins.borrow();
    for s in pv.details.album.iter().chain(&pv.details.artist) {
        match size {
            TILE => {
                s.cards.patch_cover(key, img);
                s.artists.patch_cover(key, img);
            }
            THUMB => s.tracks.patch_cover(key, img),
            _ => {}
        }
    }
}

// ------------------------------------------------------------ radio

/// The plugin behind a `plugin://` URI runs and offers a radio.
fn has_radio(ui: &Ui, uri: &str) -> bool {
    parse_plugin_uri(uri).is_some_and(|(id, _)| status(ui, &id).is_some_and(|s| s.caps.radio))
}

/// The seed of a plugin card (album, artist, playlist… of a plugin).
fn card_seed(card_id: &str) -> Option<String> {
    let (arg, _) = crate::plugins::card_target(card_id)?;
    let (id, reference, _) = crate::plugins::parse_arg(arg)?;
    Some(plugin_uri(&id, &reference))
}

/// A plugin track of a list: its seed and itself, to play first.
fn track_seed(ui: &Ui, list: &str, index: usize) -> Option<(String, TrackInfo)> {
    let st = ui.st.try_borrow().ok()?;
    let tr = st.lists.get(list)?.get(index)?;
    tr.is_plugin()
        .then(|| (tr.path.clone(), TrackInfo::from(tr)))
}

/// For the context menu: can this album card or track start a radio?
fn can_radio(ui: &Ui, is_album: bool, list: &str, index: i32, album_id: &str) -> bool {
    let seed = if is_album {
        card_seed(album_id)
    } else {
        track_seed(ui, list, index.max(0) as usize).map(|(s, _)| s)
    };
    seed.is_some_and(|s| has_radio(ui, &s))
}

/// "Start radio" on a plugin album, artist… (its card's menu).
pub fn item_radio(ui: &Rc<Ui>, id: &str, reference: &str) {
    start(ui, plugin_uri(id, reference), None);
}

/// "Start radio" on a plugin track: it plays first, the radio follows.
pub fn track_radio(ui: &Rc<Ui>, list: &str, index: usize) {
    if let Some((seed, lead)) = track_seed(ui, list, index) {
        start(ui, seed, Some(lead));
    }
}

/// "Start radio" on the artist page.
pub fn artist_radio(ui: &Rc<Ui>) {
    let seed = ui.plugins.borrow().details.artist_seed.clone();
    if let Some(seed) = seed {
        start(ui, seed, None);
    }
}

/// Replace the queue with a radio seeded by `seed` (blocking plugin call
/// on a worker thread).
fn start(ui: &Rc<Ui>, seed: String, lead: Option<TrackInfo>) {
    let app = ui.app();
    if app.get_radio_busy() {
        return;
    }
    app.set_radio_busy(true);
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.details.radio_serial += 1;
        pv.details.radio_serial
    };
    // Say so only when the plugin takes its time.
    slint::Timer::single_shot(Duration::from_millis(400), move || {
        with_ui(|ui| {
            if ui.app().get_radio_busy() && ui.plugins.borrow().details.radio_serial == serial {
                ui.toast(t("Starting radio…"), false);
            }
        })
    });
    let name = ui.ctx.ctl.plugin_name(&seed).unwrap_or_default();
    let ctl = ui.ctx.ctl.clone();
    std::thread::spawn(move || {
        let r = ctl.start_radio(&seed, lead);
        post(move |ui| {
            ui.app().set_radio_busy(false);
            match r {
                Ok(n) => ui.toast(
                    format!(
                        "{} · {}",
                        t("Radio started"),
                        crate::text::count(n, "track", "tracks")
                    ),
                    false,
                ),
                Err(e) => {
                    tracing::info!("radio from {seed}: {e}");
                    ui.toast(crate::plugins::error_text(&name, &e), true);
                }
            }
        });
    });
}

pub fn wire(ui: &Rc<Ui>) {
    ui.app().on_can_radio(|is_album, list, index, album_id| {
        let mut ok = false;
        with_ui(|ui| ok = can_radio(ui, is_album, &list, index, &album_id));
        ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ricercar_core::plugin::Fact;

    #[test]
    fn facts_read_as_one_line() {
        let mut d = ItemDetails::default();
        assert_eq!(facts_line(&d), "");
        d.facts = vec![
            Fact {
                label: "Label".into(),
                value: "Demo Records".into(),
            },
            Fact {
                label: "Released".into(),
                value: "2024".into(),
            },
        ];
        assert_eq!(facts_line(&d), "Label: Demo Records · Released: 2024");
    }

    #[test]
    fn plugin_cards_seed_a_radio() {
        let arg = crate::plugins::browse_arg("demo", "album/1", "Demo Sessions");
        let card = format!("{}{arg}", crate::plugins::ALBUM_CARD);
        assert_eq!(card_seed(&card), Some(plugin_uri("demo", "album/1")));
        assert_eq!(card_seed("local-album-id"), None);
    }
}
