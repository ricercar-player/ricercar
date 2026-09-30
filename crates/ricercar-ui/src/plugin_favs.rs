//! Favourite state of plugin items (`Item.favorite`, plugins declaring
//! `favorites`): the hearts of plugin tracks, of the plugin album and
//! artist pages and of the player bar. A click flips the heart at once,
//! `favorites.set` runs on a worker thread, and a failure puts it back.
//! Items without the field keep the old behaviour (no state shown).

use std::cell::RefCell;
use std::collections::HashMap;

use ricercar_core::plugin::{Item, parse_plugin_uri, plugin_uri};

use crate::app::{Ui, post};
use crate::text::t;

thread_local! {
    /// Known state by `plugin://` URI (tracks, albums, artists).
    static KNOWN: RefCell<HashMap<String, bool>> = RefCell::default();
    /// URIs of the plugin album and artist whose page is open.
    static ALBUM: RefCell<Option<String>> = const { RefCell::new(None) };
    static ARTIST: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Note the state the items of plugin `id` carry, when it declares
/// `favorites`.
pub fn remember(ui: &Ui, id: &str, items: &[Item]) {
    if !items.iter().any(|i| i.favorite.is_some())
        || !ui.ctx.plugins.status(id).is_some_and(|s| s.caps.favorites)
    {
        return;
    }
    KNOWN.with_borrow_mut(|k| {
        for i in items {
            if let Some(f) = i.favorite {
                k.insert(plugin_uri(id, &i.reference), f);
            }
        }
    });
}

/// State of a `plugin://` URI, `None` when unknown.
pub fn known(uri: &str) -> Option<bool> {
    KNOWN.with_borrow(|k| k.get(uri).copied())
}

/// The plugin album page shows `id`/`reference`: its heart follows the
/// known state.
pub fn show_album(ui: &Ui, id: &str, reference: &str) {
    let uri = plugin_uri(id, reference);
    ui.app().set_al_fav(known(&uri).unwrap_or(false));
    ALBUM.set(Some(uri));
}

/// Leave the album page (a local album): its heart is the library's again.
pub fn forget_album() {
    ALBUM.set(None);
}

/// A new artist page: no plugin heart until a plugin artist is found.
pub fn reset_artist(ui: &Ui) {
    ARTIST.set(None);
    let app = ui.app();
    app.set_ar_fav_shown(false);
    app.set_ar_fav(false);
}

/// A plugin artist matching the open artist page: the first one with a
/// known state gets the heart of the page.
pub fn offer_artist(ui: &Ui, id: &str, reference: &str) {
    let uri = plugin_uri(id, reference);
    let Some(f) = known(&uri) else { return };
    if ARTIST.with_borrow(Option::is_some) {
        return;
    }
    let app = ui.app();
    app.set_ar_fav_shown(true);
    app.set_ar_fav(f);
    ARTIST.set(Some(uri));
}

/// Heart of the artist page clicked.
pub fn toggle_artist(ui: &Ui) {
    if let Some(uri) = ARTIST.with_borrow(Clone::clone) {
        toggle(ui, &uri);
    }
}

/// Heart of the player bar clicked on a plugin track: nothing when its
/// state is unknown, as before.
pub fn toggle_now(ui: &Ui, uri: &str) {
    if known(uri).is_some() {
        toggle(ui, uri);
    }
}

/// Flip the heart of a plugin item. Unknown state: add it to the
/// favourites, as before the state was known.
pub fn toggle(ui: &Ui, uri: &str) {
    let Some((id, reference)) = parse_plugin_uri(uri) else {
        return;
    };
    let Some(was) = known(uri) else {
        return crate::plugins::favorite(ui, &id, &reference, true);
    };
    let on = !was;
    apply(ui, uri, on);
    let (h, uri) = (ui.ctx.plugins.clone(), uri.to_string());
    std::thread::spawn(move || {
        let r = h.favorites_set(&id, &reference, on);
        post(move |ui| match r {
            Ok(()) => ui.toast(
                if on {
                    t("Added to favorites")
                } else {
                    t("Removed from favorites")
                },
                false,
            ),
            Err(e) => {
                apply(ui, &uri, was);
                ui.toast(crate::plugins::error_message(ui, &id, &e), true);
            }
        });
    });
}

/// Show `on` everywhere the item appears.
fn apply(ui: &Ui, uri: &str, on: bool) {
    KNOWN.with_borrow_mut(|k| k.insert(uri.to_string(), on));
    for m in ui.models.track_models() {
        m.update_path(uri, |r| std::mem::replace(&mut r.fav, on) != on);
    }
    let app = ui.app();
    if ALBUM.with_borrow(|a| a.as_deref() == Some(uri)) {
        app.set_al_fav(on);
    }
    if ARTIST.with_borrow(|a| a.as_deref() == Some(uri)) {
        app.set_ar_fav(on);
    }
    if ui.ctx.ctl.lock().current_uri().as_deref() == Some(uri) {
        app.set_fav(on);
    }
}
