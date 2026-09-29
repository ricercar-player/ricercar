//! Source plugins in the interface (docs/plugins.md): settings rows,
//! browser-based sign-in, sidebar sections, the browse page, the search tab
//! and what plugins learn about the output. Plugin calls block, so they run
//! on worker threads and come back through `post`.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use ricercar_core::library::Track;
use ricercar_core::plugin::{
    AuthState, Item, ItemKind, PluginError, PluginStatus, RunState, catalog,
};
use ricercar_core::{EnqueueAt, PlayContext, TrackInfo};
use slint::{Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::app::{RowOpts, TILE, Ui, post, set_rows, track_rows, with_ui};
use crate::images::Source;
use crate::text::t;
use crate::{AlbumCard, CatalogRow, Page, PluginNav, PluginRow};

/// Separates the parts of a browse argument / plugin card id.
const SEP: char = '\u{1f}';
/// Prefix of the album-card ids of plugin items.
pub const CARD: &str = "plugin\u{1f}";
/// Items asked per page (the protocol allows 200).
const PAGE: usize = 100;
/// Most tracks gathered to play a plugin album or playlist.
const PLAY_MAX: usize = 1000;

#[derive(Default)]
pub struct PluginsView {
    rev: Option<u64>,
    /// Sidebar sections of signed-in plugins, by plugin id.
    sections: HashMap<String, Vec<Item>>,
    loading_sections: std::collections::HashSet<String>,
    /// Plugin whose sign-in dialog is open, and its address.
    signin: Option<String>,
    signin_url: String,
    /// Browse page: plugin id, ref, next offset.
    browse: Option<(String, String, usize)>,
    browse_serial: u64,
    /// Plugin ids behind the search tabs (index 1…).
    search_ids: Vec<String>,
    search_serial: u64,
    /// Item being resolved and since when (loading state after 300 ms).
    resolving: Option<(u64, Instant)>,
    /// Community catalogue as last read, and the read in flight.
    catalog: Vec<catalog::Entry>,
    catalog_serial: u64,
    /// Entry waiting for the install confirmation.
    pending_install: Option<catalog::Entry>,
    /// Index to read instead of the hub's (the headless tour).
    pub index_override: Option<String>,
}

pub fn browse_arg(id: &str, reference: &str, title: &str) -> String {
    format!("{id}{SEP}{reference}{SEP}{title}")
}

fn parse_arg(arg: &str) -> Option<(String, String, String)> {
    let mut it = arg.splitn(3, SEP);
    Some((
        it.next()?.to_string(),
        it.next()?.to_string(),
        it.next().unwrap_or("").to_string(),
    ))
}

fn host(ui: &Ui) -> ricercar_core::plugin::PluginHost {
    ui.ctx.plugins.clone()
}

/// Plain-text message for an error (never the plugin's markup).
fn error_text(name: &str, e: &PluginError) -> String {
    match e {
        PluginError::AuthRequired => format!("{} {name}", t("Sign in to")),
        PluginError::NotFound => t("Not found").into(),
        PluginError::Unavailable => t("Not available (region, subscription or format)").into(),
        PluginError::RateLimited { retry_after } => {
            format!(
                "{} ({retry_after} s)",
                t("Too many requests, try again later")
            )
        }
        PluginError::Network => t("Offline: the service could not be reached").into(),
        PluginError::NotRunning => format!("{name}: {}", t("not running")),
        PluginError::Timeout => format!("{name}: {}", t("no answer")),
        PluginError::Other { message, .. } => format!("{name}: {message}"),
    }
}

fn name_of(ui: &Ui, id: &str) -> String {
    host(ui)
        .status(id)
        .map(|s| s.name)
        .unwrap_or_else(|| id.to_string())
}

// ------------------------------------------------------------ polling

/// From the player tick: follow plugin status changes and the resolving
/// state of the current item.
pub fn poll(ui: &Rc<Ui>) {
    let resolving = ui.ctx.ctl.lock().resolving;
    let show = {
        let mut pv = ui.plugins.borrow_mut();
        pv.resolving = match (resolving, pv.resolving) {
            (Some(id), Some((old, since))) if id == old => Some((old, since)),
            (Some(id), _) => Some((id, Instant::now())),
            (None, _) => None,
        };
        pv.resolving
            .is_some_and(|(_, since)| since.elapsed().as_millis() >= 300)
    };
    if ui.app().get_resolving() != show {
        ui.app().set_resolving(show);
    }

    let rev = host(ui).revision();
    if ui.plugins.borrow().rev == Some(rev) {
        return;
    }
    ui.plugins.borrow_mut().rev = Some(rev);
    let statuses = host(ui).statuses();
    refresh_rows(ui, &statuses);
    refresh_sections(ui, &statuses);
    refresh_search_sources(ui, &statuses);
    let open = ui.plugins.borrow().signin.clone();
    if let Some(id) = open
        && statuses.iter().any(|s| s.id == id && s.signed_in())
    {
        close_sign_in(ui);
        ui.toast(format!("{} {}", t("Signed in to"), name_of(ui, &id)), false);
        reload_browse(ui);
    }
}

fn status_text(s: &PluginStatus) -> (String, i32) {
    match &s.state {
        RunState::Disabled => (t("Off").into(), 0),
        RunState::Starting => (t("Starting…").into(), 1),
        RunState::Restarting { in_secs } => (format!("{} {in_secs} s", t("Restarting in")), 1),
        RunState::Failed(m) => (format!("{}: {m}", t("Stopped")), 2),
        RunState::Running if !s.caps.auth => (t("Ready").into(), 0),
        RunState::Running => match &s.auth {
            Some(a) if a.state == AuthState::SignedIn => match &a.account {
                Some(acc) => {
                    let detail = acc
                        .detail
                        .as_ref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    (
                        format!("{} {}{detail}", t("Signed in as"), acc.display_name),
                        0,
                    )
                }
                None => (t("Signed in").into(), 0),
            },
            Some(a) if a.state == AuthState::Expired => (t("Sign-in expired").into(), 2),
            _ => (t("Signed out").into(), 0),
        },
    }
}

fn refresh_rows(ui: &Ui, statuses: &[PluginStatus]) {
    let declared: HashMap<String, ricercar_core::config::PluginConfig> = ui
        .ctx
        .config
        .read()
        .unwrap()
        .plugins
        .iter()
        .map(|p| (p.id.clone(), p.clone()))
        .collect();
    let pv = ui.plugins.borrow();
    let rows: Vec<PluginRow> = statuses
        .iter()
        .map(|s| {
            let (status, state) = status_text(s);
            PluginRow {
                id: s.id.clone().into(),
                name: s.name.clone().into(),
                detail: if s.version.is_empty() {
                    s.id.clone()
                } else {
                    format!("{} · {}", s.id, s.version)
                }
                .into(),
                status: status.into(),
                state,
                enabled: declared.get(&s.id).is_some_and(|d| d.enabled),
                auth: s.caps.auth,
                signed_in: s.caps.auth && s.signed_in(),
                from_hub: declared.get(&s.id).is_some_and(|d| d.version.is_some()),
                update: declared.get(&s.id).is_some_and(|d| {
                    pv.catalog
                        .iter()
                        .find(|e| e.id == s.id)
                        .is_some_and(|e| e.installable() && catalog::update_available(d, e))
                }),
            }
        })
        .collect();
    drop(pv);
    ui.app().set_plugin_rows(ModelRc::new(VecModel::from(rows)));
}

/// Sidebar: each signed-in plugin that browses, with its `browse.root`.
fn refresh_sections(ui: &Rc<Ui>, statuses: &[PluginStatus]) {
    let ready: Vec<&PluginStatus> = statuses
        .iter()
        .filter(|s| s.signed_in() && s.caps.browse)
        .collect();
    {
        let mut pv = ui.plugins.borrow_mut();
        pv.sections
            .retain(|id, _| ready.iter().any(|s| &s.id == id));
    }
    for s in &ready {
        let pending = {
            let pv = ui.plugins.borrow();
            pv.sections.contains_key(&s.id) || pv.loading_sections.contains(&s.id)
        };
        if pending {
            continue;
        }
        ui.plugins
            .borrow_mut()
            .loading_sections
            .insert(s.id.clone());
        let (h, id) = (host(ui), s.id.clone());
        std::thread::spawn(move || {
            let r = h.browse_root(&id);
            post(move |ui| {
                ui.plugins.borrow_mut().loading_sections.remove(&id);
                match r {
                    Ok(items) => {
                        ui.plugins.borrow_mut().sections.insert(id, items);
                        rebuild_nav(ui);
                    }
                    Err(e) => tracing::info!("plugin[{id}] browse.root: {e}"),
                }
            });
        });
    }
    rebuild_nav(ui);
}

fn kind_name(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Track => "track",
        ItemKind::Album => "album",
        ItemKind::Artist => "artist",
        ItemKind::Playlist => "playlist",
        ItemKind::Folder => "folder",
    }
}

fn rebuild_nav(ui: &Ui) {
    let order: Vec<(String, String)> = host(ui)
        .statuses()
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let pv = ui.plugins.borrow();
    let mut rows = Vec::new();
    for (id, name) in order {
        let Some(sections) = pv.sections.get(&id) else {
            continue;
        };
        rows.push(PluginNav {
            header: true,
            title: name.into(),
            ..Default::default()
        });
        for it in sections.iter().filter(|i| i.is_browsable()).take(12) {
            rows.push(PluginNav {
                header: false,
                title: it.title.clone().into(),
                arg: browse_arg(&id, &it.reference, &it.title).into(),
                kind: kind_name(it.kind).into(),
            });
        }
    }
    ui.app().set_plugin_nav(ModelRc::new(VecModel::from(rows)));
}

fn refresh_search_sources(ui: &Ui, statuses: &[PluginStatus]) {
    let usable: Vec<&PluginStatus> = statuses
        .iter()
        .filter(|s| s.signed_in() && s.caps.search)
        .collect();
    let mut names: Vec<slint::SharedString> = vec![t("Library").into()];
    names.extend(
        usable
            .iter()
            .map(|s| slint::SharedString::from(s.name.as_str())),
    );
    let ids: Vec<String> = usable.iter().map(|s| s.id.clone()).collect();
    let app = ui.app();
    // Keep the selected plugin tab if it is still there.
    let current = {
        let pv = ui.plugins.borrow();
        let i = app.get_search_source();
        (i > 0)
            .then(|| pv.search_ids.get(i as usize - 1).cloned())
            .flatten()
    };
    let selected = current
        .and_then(|c| ids.iter().position(|i| *i == c))
        .map(|p| p as i32 + 1)
        .unwrap_or(0);
    ui.plugins.borrow_mut().search_ids = ids;
    app.set_search_sources(ModelRc::new(VecModel::from(names)));
    app.set_search_source(selected);
}

// ------------------------------------------------------------ sign-in

fn qr_image(text: &str) -> slint::Image {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return slint::Image::default();
    };
    let w = code.width();
    let quiet = 2;
    let scale = 4;
    let side = (w + 2 * quiet) * scale;
    let colors = code.to_colors();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(side as u32, side as u32);
    let px = buf.make_mut_slice();
    for y in 0..side {
        for x in 0..side {
            let (mx, my) = (
                (x / scale) as isize - quiet as isize,
                (y / scale) as isize - quiet as isize,
            );
            let dark = mx >= 0
                && my >= 0
                && (mx as usize) < w
                && (my as usize) < w
                && colors[my as usize * w + mx as usize] == qrcode::Color::Dark;
            let v = if dark { 0 } else { 255 };
            px[y * side + x] = Rgba8Pixel {
                r: v,
                g: v,
                b: v,
                a: 255,
            };
        }
    }
    slint::Image::from_rgba8(buf)
}

fn open_url(url: &str) {
    // The headless tours never open a browser.
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

pub fn begin_sign_in(ui: &Ui, id: &str) {
    {
        let mut pv = ui.plugins.borrow_mut();
        pv.signin = Some(id.to_string());
        pv.signin_url.clear();
    }
    let app = ui.app();
    app.set_signin_title(format!("{} {}", t("Sign in to"), name_of(ui, id)).into());
    app.set_signin_url("".into());
    app.set_signin_instructions("".into());
    app.set_signin_status("".into());
    app.set_signin_input("".into());
    app.set_signin_qr(slint::Image::default());
    app.set_signin_expects_input(true);
    app.set_signin_busy(false);
    app.set_signin_open(true);
    let (h, id) = (host(ui), id.to_string());
    std::thread::spawn(move || {
        let r = h.auth_begin(&id);
        post(move |ui| {
            if ui.plugins.borrow().signin.as_deref() != Some(id.as_str()) {
                return;
            }
            let app = ui.app();
            match r {
                Ok(b) if b.url.starts_with("http://") || b.url.starts_with("https://") => {
                    open_url(&b.url);
                    app.set_signin_url(b.url.clone().into());
                    app.set_signin_qr(qr_image(&b.url));
                    app.set_signin_instructions(b.instructions.unwrap_or_default().into());
                    app.set_signin_expects_input(b.expects_input);
                    ui.plugins.borrow_mut().signin_url = b.url;
                }
                Ok(_) => app.set_signin_status(t("The plugin gave an unusable address.").into()),
                Err(e) => app.set_signin_status(error_text(&name_of(ui, &id), &e).into()),
            }
        });
    });
}

pub fn complete_sign_in(ui: &Ui, input: &str) {
    let Some(id) = ui.plugins.borrow().signin.clone() else {
        return;
    };
    ui.app().set_signin_busy(true);
    ui.app().set_signin_status("".into());
    let (h, input) = (host(ui), input.trim().to_string());
    std::thread::spawn(move || {
        let r = h.auth_complete(&id, &input);
        post(move |ui| {
            if ui.plugins.borrow().signin.as_deref() != Some(id.as_str()) {
                return;
            }
            ui.app().set_signin_busy(false);
            match r {
                Ok(st) if st.state == AuthState::SignedIn => {
                    close_sign_in(ui);
                    ui.toast(format!("{} {}", t("Signed in to"), name_of(ui, &id)), false);
                    reload_browse(ui);
                }
                Ok(_) => ui.app().set_signin_status(
                    t("Not signed in yet. Check the code and try again.").into(),
                ),
                Err(e) => ui
                    .app()
                    .set_signin_status(error_text(&name_of(ui, &id), &e).into()),
            }
        });
    });
}

fn close_sign_in(ui: &Ui) {
    ui.plugins.borrow_mut().signin = None;
    ui.app().set_signin_open(false);
}

// ------------------------------------------------------------ browse page

fn card(ui: &Ui, id: &str, it: &Item) -> AlbumCard {
    let (cover, ckey) = match it.art.clone().filter(|a| a.starts_with("http")) {
        Some(url) => (ui.cover(&url, Source::Url(url.clone()), None, TILE), url),
        None => (slint::Image::default(), String::new()),
    };
    let hires = it
        .format
        .as_ref()
        .is_some_and(|f| ricercar_core::library::is_hires(f.sample_rate, f.bits));
    AlbumCard {
        id: format!("{CARD}{}", browse_arg(id, &it.reference, &it.title)).into(),
        title: it.title.clone().into(),
        artist: it
            .subtitle
            .clone()
            .or(it.artist.clone())
            .unwrap_or_default()
            .into(),
        year: String::new().into(),
        cover,
        ckey: ckey.into(),
        hires,
        fav: false,
        plugin: true,
    }
}

fn tracks_of(id: &str, items: &[Item]) -> Vec<Track> {
    items
        .iter()
        .filter(|i| i.kind == ItemKind::Track)
        .map(|i| Track::from_info(&i.to_track_info(id)))
        .collect()
}

fn row_opts(start: usize) -> RowOpts {
    RowOpts {
        cover: true,
        eager: true,
        track_numbers: false,
        discs: false,
        album_artist: None,
        start,
    }
}

pub fn load_browse(ui: &Rc<Ui>, arg: &str) {
    let app = ui.app();
    app.set_br_arg(arg.into());
    set_rows(&ui.models.br_cards, Vec::new());
    set_rows(&ui.models.br_tracks, Vec::new());
    ui.st.borrow_mut().lists.insert("browse".into(), Vec::new());
    app.set_br_error("".into());
    app.set_br_auth(false);
    app.set_br_more(false);
    let Some((id, reference, title)) = parse_arg(arg) else {
        return;
    };
    app.set_br_title(title.into());
    app.set_br_sub(name_of(ui, &id).into());
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.browse_serial += 1;
        pv.browse = Some((id.clone(), reference.clone(), 0));
        pv.browse_serial
    };
    fetch_page(ui, serial, id, reference, 0);
}

fn reload_browse(ui: &Rc<Ui>) {
    if ui.app().get_page() == Page::Browse {
        let arg = ui.app().get_br_arg().to_string();
        load_browse(ui, &arg);
    }
}

fn fetch_page(ui: &Ui, serial: u64, id: String, reference: String, offset: usize) {
    ui.app().set_br_loading(true);
    let h = host(ui);
    std::thread::spawn(move || {
        let r = h.browse_list(&id, &reference, offset, PAGE);
        post(move |ui| {
            if ui.plugins.borrow().browse_serial != serial {
                return;
            }
            let app = ui.app();
            app.set_br_loading(false);
            match r {
                Ok((items, _, more)) => {
                    let cards: Vec<AlbumCard> = items
                        .iter()
                        .filter(|i| i.kind != ItemKind::Track && i.is_browsable())
                        .map(|i| card(ui, &id, i))
                        .collect();
                    ui.models.br_cards.extend(cards);
                    let tracks = tracks_of(&id, &items);
                    let rows = track_rows(ui, &tracks, row_opts(ui.models.br_tracks.row_count()));
                    ui.models.br_tracks.extend(rows);
                    if let Some(list) = ui.st.borrow_mut().lists.get_mut("browse") {
                        list.extend(tracks);
                    }
                    app.set_br_more(more);
                    if let Some(b) = ui.plugins.borrow_mut().browse.as_mut() {
                        b.2 = offset + items.len();
                    }
                }
                Err(e) => {
                    app.set_br_auth(e == PluginError::AuthRequired);
                    app.set_br_error(error_text(&name_of(ui, &id), &e).into());
                    app.set_br_more(false);
                }
            }
        });
    });
}

fn load_more(ui: &Ui) {
    let (serial, b) = {
        let pv = ui.plugins.borrow();
        (pv.browse_serial, pv.browse.clone())
    };
    if let Some((id, reference, offset)) = b {
        fetch_page(ui, serial, id, reference, offset);
    }
}

/// Every track under a plugin item (pages, up to `PLAY_MAX`).
fn all_tracks(
    h: &ricercar_core::plugin::PluginHost,
    id: &str,
    reference: &str,
) -> Result<Vec<TrackInfo>, PluginError> {
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let (items, _, more) = h.browse_list(id, reference, offset, 200)?;
        offset += items.len();
        out.extend(
            items
                .iter()
                .filter(|i| i.is_playable())
                .map(|i| i.to_track_info(id)),
        );
        if !more || items.is_empty() || out.len() >= PLAY_MAX {
            return Ok(out);
        }
    }
}

/// Context-menu and double-click actions on a plugin card.
pub fn card_action(ui: &Rc<Ui>, arg: &str, action: &str) {
    let Some((id, reference, _)) = parse_arg(arg) else {
        return;
    };
    match action {
        "open" => ui.navigate(Page::Browse, arg, true),
        "fav" => favorite(ui, &id, &reference, true),
        "play" | "shuffle" | "next" | "queue" => {}
        a if a.starts_with("pl:") => {}
        _ => return,
    }
    if action == "open" || action == "fav" {
        return;
    }
    let (h, action) = (host(ui), action.to_string());
    std::thread::spawn(move || {
        let r = all_tracks(&h, &id, &reference);
        post(move |ui| match r {
            Ok(infos) if !infos.is_empty() => {
                let ctl = &ui.ctx.ctl;
                match action.as_str() {
                    "play" => ctl.play_tracks(infos, 0, PlayContext::None),
                    "shuffle" => ctl.play_shuffled(infos, PlayContext::None),
                    "next" => {
                        ctl.enqueue(infos, EnqueueAt::Next);
                        ui.toast(t("Will play next"), false);
                    }
                    "queue" => {
                        ctl.enqueue(infos, EnqueueAt::End);
                        ui.toast(t("Added to the queue"), false);
                    }
                    a => {
                        let tracks: Vec<Track> = infos.iter().map(Track::from_info).collect();
                        crate::views::add_to_playlist(ui, &a[3..], &tracks);
                    }
                }
            }
            Ok(_) => ui.toast(t("Nothing playable here"), true),
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

/// Favourite on the service (plugins with the `favorites` capability).
pub fn favorite(ui: &Ui, id: &str, reference: &str, on: bool) {
    if !host(ui).status(id).is_some_and(|s| s.caps.favorites) {
        ui.toast(t("This plugin has no favourites"), true);
        return;
    }
    let (h, id, reference) = (host(ui), id.to_string(), reference.to_string());
    std::thread::spawn(move || {
        let r = h.favorites_set(&id, &reference, on);
        post(move |ui| match r {
            Ok(()) => ui.toast(t("Added to favorites"), false),
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

/// Favourite toggle on a plugin track of any list.
pub fn favorite_track(ui: &Ui, tr: &Track) {
    if let Some((id, reference)) = ricercar_core::plugin::parse_plugin_uri(&tr.path) {
        favorite(ui, &id, &reference, true);
    }
}

fn play_page(ui: &Rc<Ui>, shuffle: bool) {
    let tracks = ui
        .st
        .borrow()
        .lists
        .get("browse")
        .cloned()
        .unwrap_or_default();
    let infos: Vec<TrackInfo> = tracks.iter().map(TrackInfo::from).collect();
    if infos.is_empty() {
        return;
    }
    if shuffle {
        ui.ctx.ctl.play_shuffled(infos, PlayContext::None);
    } else {
        ui.ctx.ctl.play_tracks(infos, 0, PlayContext::None);
    }
}

// ------------------------------------------------------------ search

/// Search the plugin of the selected tab (the library tab is `views`).
pub fn run_search(ui: &Rc<Ui>, q: &str) {
    let i = ui.app().get_search_source();
    let Some(id) = ui
        .plugins
        .borrow()
        .search_ids
        .get((i - 1).max(0) as usize)
        .cloned()
    else {
        return;
    };
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.search_serial += 1;
        pv.search_serial
    };
    let app = ui.app();
    app.set_ps_error("".into());
    if q.trim().is_empty() {
        set_rows(&ui.models.ps_cards, Vec::new());
        set_rows(&ui.models.ps_tracks, Vec::new());
        return;
    }
    app.set_ps_loading(true);
    let (h, q) = (host(ui), q.to_string());
    std::thread::spawn(move || {
        let r = h.search(&id, &q, 0, 50);
        post(move |ui| {
            if ui.plugins.borrow().search_serial != serial {
                return;
            }
            let app = ui.app();
            app.set_ps_loading(false);
            match r {
                Ok(groups) => {
                    let items: Vec<Item> = groups.into_iter().flat_map(|g| g.1).collect();
                    let cards = items
                        .iter()
                        .filter(|i| i.kind != ItemKind::Track && i.is_browsable())
                        .map(|i| card(ui, &id, i))
                        .collect();
                    set_rows(&ui.models.ps_cards, cards);
                    let tracks = tracks_of(&id, &items);
                    set_rows(&ui.models.ps_tracks, track_rows(ui, &tracks, row_opts(0)));
                    ui.st.borrow_mut().lists.insert("psearch".into(), tracks);
                    if ui.models.ps_cards.row_count() == 0 && ui.models.ps_tracks.row_count() == 0 {
                        app.set_ps_error(t("No results").into());
                    }
                }
                Err(e) => {
                    set_rows(&ui.models.ps_cards, Vec::new());
                    set_rows(&ui.models.ps_tracks, Vec::new());
                    app.set_ps_error(error_text(&name_of(ui, &id), &e).into());
                }
            }
        });
    });
}

// ------------------------------------------------------------ catalogue

/// Hub index to read, and whether local files are allowed (only for an
/// index given by the tour or `RICERCAR_PLUGIN_INDEX`).
fn index_source(ui: &Ui) -> (String, bool) {
    if let Some(o) = ui.plugins.borrow().index_override.clone() {
        return (o, true);
    }
    match std::env::var("RICERCAR_PLUGIN_INDEX") {
        Ok(v) if !v.is_empty() => (v, true),
        _ => (catalog::INDEX_URL.to_string(), false),
    }
}

/// The Plugins page: installed plugins, then the community catalogue
/// (read from the hub only when the page opens, if allowed).
pub fn load_page(ui: &Rc<Ui>) {
    ui.plugins.borrow_mut().rev = None;
    poll(ui);
    if !ui.ctx.config.read().unwrap().online.plugin_catalog {
        ui.plugins.borrow_mut().catalog.clear();
        ui.app().set_catalog_status(
            t("The community catalogue is off (Settings → Online extras).").into(),
        );
        refresh_catalog_rows(ui);
        return;
    }
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.catalog_serial += 1;
        pv.catalog_serial
    };
    ui.app()
        .set_catalog_status(t("Loading the catalogue…").into());
    let (url, local) = index_source(ui);
    std::thread::spawn(move || {
        let r = catalog::fetch_index(&url, local);
        post(move |ui| {
            if ui.plugins.borrow().catalog_serial != serial {
                return;
            }
            match r {
                Ok(list) => {
                    ui.app().set_catalog_status(
                        if list.is_empty() {
                            t("The catalogue is empty for now.")
                        } else {
                            ""
                        }
                        .into(),
                    );
                    ui.plugins.borrow_mut().catalog = list;
                }
                Err(e) => ui.app().set_catalog_status(
                    format!("{}: {e}", t("Could not read the catalogue")).into(),
                ),
            }
            refresh_catalog_rows(ui);
            let statuses = host(ui).statuses();
            refresh_rows(ui, &statuses);
        });
    });
}

fn capability_names(caps: &[String]) -> String {
    caps.iter()
        .filter_map(|c| match c.as_str() {
            "auth" => Some(t("sign-in")),
            "browse" => Some(t("browse")),
            "search" => Some(t("search")),
            "favorites" => Some(t("favourites")),
            "remote_control" => Some(t("remote control")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn refresh_catalog_rows(ui: &Ui) {
    let filter = ui.app().get_catalog_filter().to_lowercase();
    let installed: Vec<String> = ui
        .ctx
        .config
        .read()
        .unwrap()
        .plugins
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let rows: Vec<CatalogRow> = ui
        .plugins
        .borrow()
        .catalog
        .iter()
        .filter(|e| {
            filter.is_empty()
                || [&e.name, &e.description, &e.author, &e.id]
                    .iter()
                    .any(|f| f.to_lowercase().contains(filter.trim()))
        })
        .map(|e| {
            let mut by = Vec::new();
            if !e.author.is_empty() {
                by.push(format!("{} {}", t("by"), e.author));
            }
            if !e.license.is_empty() {
                by.push(e.license.clone());
            }
            if !e.version.is_empty() {
                by.push(e.version.clone());
            }
            CatalogRow {
                id: e.id.clone().into(),
                name: e.name.chars().take(80).collect::<String>().into(),
                byline: by.join(" · ").into(),
                description: e.description.chars().take(400).collect::<String>().into(),
                caps: capability_names(&e.capabilities).into(),
                state: if installed.contains(&e.id) {
                    1
                } else if e.installable() {
                    0
                } else {
                    2
                },
            }
        })
        .collect();
    ui.app()
        .set_catalog_rows(ModelRc::new(VecModel::from(rows)));
}

fn entry(ui: &Ui, id: &str) -> Option<catalog::Entry> {
    ui.plugins
        .borrow()
        .catalog
        .iter()
        .find(|e| e.id == id)
        .cloned()
}

/// Ask before downloading and running anything.
pub fn confirm_install(ui: &Ui, id: &str, update: bool) {
    let Some(e) = entry(ui, id) else { return };
    let Some(asset) = e.asset_for(catalog::current_arch()) else {
        return;
    };
    let host_name = asset
        .url
        .split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or(t("this computer"))
        .to_string();
    let app = ui.app();
    app.set_install_title(
        if update {
            format!("{} {}?", t("Update"), e.name)
        } else {
            format!("{} {}?", t("Install"), e.name)
        }
        .into(),
    );
    app.set_install_body(
        crate::text::install_body(&e.name, &e.version, &e.author, &host_name, &e.repository).into(),
    );
    app.set_install_busy(false);
    ui.plugins.borrow_mut().pending_install = Some(e);
    app.set_install_open(true);
}

pub fn do_install(ui: &Ui) {
    let Some(e) = ui.plugins.borrow().pending_install.clone() else {
        return;
    };
    ui.app().set_install_busy(true);
    let (_, local) = index_source(ui);
    let root = catalog::bin_root(&ricercar_core::config::data_dir());
    std::thread::spawn(move || {
        let r = catalog::install(&e, &root, local);
        post(move |ui| {
            ui.app().set_install_busy(false);
            ui.app().set_install_open(false);
            ui.plugins.borrow_mut().pending_install = None;
            match r {
                Ok(cfg) => {
                    ui.ctx
                        .update_config(|c| match c.plugins.iter_mut().find(|p| p.id == cfg.id) {
                            Some(p) => *p = cfg.clone(),
                            None => c.plugins.push(cfg.clone()),
                        });
                    ui.toast(format!("{} {}", t("Installed"), e.name), false);
                }
                Err(err) => ui.toast(format!("{}: {err}", e.name), true),
            }
            ui.plugins.borrow_mut().rev = None;
            refresh_catalog_rows(ui);
        });
    });
}

/// Remove a plugin: its declaration, and its binaries when the hub
/// installed it (a plugin declared by hand keeps its files). Its data
/// directory, which the plugin owns, stays.
fn remove(ui: &Ui, id: &str) {
    let declared = ui
        .ctx
        .config
        .read()
        .unwrap()
        .plugins
        .iter()
        .find(|p| p.id == id)
        .cloned();
    let Some(d) = declared else { return };
    ui.ctx.update_config(|c| c.plugins.retain(|p| p.id != id));
    if d.version.is_some() {
        let root = catalog::bin_root(&ricercar_core::config::data_dir());
        let id = id.to_string();
        // After the host has had time to stop it.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            if let Err(e) = catalog::uninstall(&root, &id) {
                tracing::warn!("uninstall plugin {id}: {e}");
            }
        });
    }
    ui.toast(format!("{} {}", t("Removed"), name_of(ui, id)), false);
    ui.plugins.borrow_mut().rev = None;
    refresh_catalog_rows(ui);
}

// ------------------------------------------------------------ output

/// Tell plugins what the output takes natively (probed capabilities when
/// known), so they pick a stream format the DAC plays as is.
pub fn push_output(ui: &Ui) {
    let device = ui.ctx.ctl.device_name();
    let caps = ui.saved_state.borrow().dac_caps.get(&device).map(|c| {
        let deep = c.formats.iter().any(|f| f != "S16_LE");
        (c.rates.clone(), Some(if deep { 24 } else { 16 }))
    });
    host(ui).set_output(ricercar_daemon::output_info(&device, caps));
}

// ------------------------------------------------------------ wiring

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_plugin_toggle(|id, on| {
        with_ui(|ui| {
            let id = id.to_string();
            ui.ctx.update_config(|c| {
                if let Some(p) = c.plugins.iter_mut().find(|p| p.id == id) {
                    p.enabled = on;
                }
            });
        })
    });
    app.on_plugin_sign_in(|id| with_ui(|ui| begin_sign_in(ui, &id)));
    app.on_plugin_sign_out(|id| {
        with_ui(|ui| {
            let (h, id) = (host(ui), id.to_string());
            std::thread::spawn(move || {
                let r = h.auth_sign_out(&id);
                post(move |ui| {
                    if let Err(e) = r {
                        ui.toast(error_text(&name_of(ui, &id), &e), true);
                    }
                });
            });
        })
    });
    app.on_signin_open_browser(|| {
        with_ui(|ui| {
            let url = ui.plugins.borrow().signin_url.clone();
            open_url(&url);
        })
    });
    app.on_signin_complete(|input| with_ui(|ui| complete_sign_in(ui, &input)));
    app.on_signin_cancel(|| with_ui(|ui| close_sign_in(ui)));
    app.on_br_load_more(|| with_ui(|ui| load_more(ui)));
    app.on_br_sign_in(|| {
        with_ui(|ui| {
            let id = ui.plugins.borrow().browse.as_ref().map(|b| b.0.clone());
            if let Some(id) = id {
                begin_sign_in(ui, &id);
            }
        })
    });
    app.on_br_play(|shuffle| with_ui(|ui| play_page(ui, shuffle)));
    app.on_plugin_update(|id| with_ui(|ui| confirm_install(ui, &id, true)));
    app.on_plugin_remove(|id| with_ui(|ui| remove(ui, &id)));
    app.on_catalog_install(|id| with_ui(|ui| confirm_install(ui, &id, false)));
    app.on_catalog_filter_edited(|_| with_ui(|ui| refresh_catalog_rows(ui)));
    app.on_catalog_open_repo(|id| {
        with_ui(|ui| {
            if let Some(e) = entry(ui, &id) {
                open_url(&e.repository);
            }
        })
    });
    app.on_open_hub(|| open_url("https://github.com/ricercar-player/ricercar-plugins"));
    app.on_install_confirm(|| with_ui(|ui| do_install(ui)));
    app.on_install_cancel(|| {
        with_ui(|ui| {
            ui.plugins.borrow_mut().pending_install = None;
            ui.app().set_install_open(false);
        })
    });
    app.on_search_source_changed(|| {
        with_ui(|ui| {
            let q = ui.app().get_search_text().to_string();
            crate::views::run_search(ui, &q);
        })
    });
    push_output(ui);
    poll(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_args_roundtrip() {
        let a = browse_arg("demo", "album/1", "Demo · Sessions");
        assert_eq!(
            parse_arg(&a),
            Some(("demo".into(), "album/1".into(), "Demo · Sessions".into()))
        );
        assert_eq!(parse_arg("demo"), None);
        let card_id = format!("{CARD}{a}");
        assert_eq!(card_id.strip_prefix(CARD), Some(a.as_str()));
    }

    #[test]
    fn errors_read_as_plain_text() {
        assert_eq!(
            error_text("Demo", &PluginError::AuthRequired),
            "Sign in to Demo"
        );
        assert!(
            error_text(
                "Demo",
                &PluginError::Other {
                    code: 1,
                    message: "<b>x</b>".into()
                }
            )
            .contains("<b>x</b>")
        );
    }
}
