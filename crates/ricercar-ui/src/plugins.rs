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
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::app::{RowOpts, TILE, Ui, post, set_rows, track_rows, with_ui};
use crate::images::Source;
use crate::text::{t, tf};
use crate::{AlbumCard, ArtistCard, CatalogRow, Page, PluginNav, PluginRow};

/// Separates the parts of a browse argument / plugin card id.
const SEP: char = '\u{1f}';
/// Prefix of the album-card ids of plugin items (browse page)…
pub const CARD: &str = "plugin\u{1f}";
/// …and of plugin albums (album page).
pub const ALBUM_CARD: &str = "plugin-album\u{1f}";

/// The browse argument behind a plugin card id, and whether it is an album.
pub fn card_target(id: &str) -> Option<(&str, bool)> {
    id.strip_prefix(ALBUM_CARD)
        .map(|a| (a, true))
        .or_else(|| id.strip_prefix(CARD).map(|a| (a, false)))
}
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
    pub browse_serial: u64,
    /// Entry ids of the browse page's tracks (a plugin playlist), in the
    /// order of its track list.
    browse_entries: Vec<Option<String>>,
    /// Plugins offered by the new-playlist dialog after "Library" (index 1…).
    new_locations: Vec<String>,
    /// Plugin ids behind the search scopes after "Everything" and "My
    /// library" (index 2…).
    scope_ids: Vec<String>,
    /// Global search: local results and each signed-in plugin's part.
    search_local: Option<LocalResults>,
    search: Vec<SearchPart>,
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
    /// Library lists of signed-in plugins with `library`, in declaration
    /// order, and the ones being read.
    libs: Vec<PluginLib>,
    loading_libs: std::collections::HashSet<String>,
    /// Cover key (art URL) of the plugin album on the album page.
    pub album_art: Option<String>,
    /// `home` entries of each plugin's `browse.root`, and the Home shelves
    /// read from them (plugins with `library` only).
    home: HashMap<String, Vec<Item>>,
    pub home_shelves: Vec<HomeShelfRows>,
    loading_home: std::collections::HashSet<String>,
    /// The settings dialog, when open.
    pub settings: Option<crate::plugin_settings::Open>,
    /// Items seen in lists that carry refs or actions, by (plugin id, ref):
    /// what the context menus and the queue need once the item became a
    /// plain track or card (see `plugin_menu`).
    pub known: HashMap<(String, String), Item>,
    /// The plugin item under the open context menu, and the serial of the
    /// "play" action being read for it.
    pub menu: Option<(String, Item)>,
    pub action_serial: u64,
    /// Details on the artist and album pages, radios.
    pub details: crate::details::DetailsView,
}

impl PluginsView {
    /// A plugin item seen this session: remembered from a list, else in
    /// the plugin's library, sidebar sections or search results.
    pub fn lookup(&self, id: &str, reference: &str) -> Option<Item> {
        let hit = |list: &[Item]| list.iter().find(|i| i.reference == reference).cloned();
        if let Some(it) = self.known.get(&(id.to_string(), reference.to_string())) {
            return Some(it.clone());
        }
        let lib = self.libs.iter().filter(|l| l.id == id).find_map(|l| {
            hit(&l.albums)
                .or_else(|| hit(&l.artists))
                .or_else(|| hit(&l.tracks))
                .or_else(|| hit(&l.playlists))
        });
        lib.or_else(|| self.sections.get(id).and_then(|s| hit(s)))
            .or_else(|| {
                self.search.iter().filter(|p| p.id == id).find_map(|p| {
                    hit(&p.artists)
                        .or_else(|| hit(&p.albums))
                        .or_else(|| hit(&p.playlists))
                })
            })
    }
}

/// A Home shelf from a plugin: plugin id, title, cards.
pub struct HomeShelfRows {
    id: String,
    title: String,
    source: String,
    pub cards: crate::app::Rows<AlbumCard>,
}

/// Local results of the global search.
#[derive(Clone, Default)]
pub struct LocalResults {
    pub artists: Vec<ArtistCard>,
    pub albums: Vec<AlbumCard>,
    pub playlists: Vec<AlbumCard>,
    pub tracks: Vec<Track>,
}

/// One plugin's search results: its catalogue search, or matches in its
/// library list when it cannot search.
#[derive(Clone, Default)]
struct SearchPart {
    id: String,
    name: String,
    /// Matches in the user's library on the service (favourites,
    /// playlists), shown before the catalogue's results.
    mine: bool,
    pending: bool,
    error: Option<String>,
    artists: Vec<Item>,
    albums: Vec<Item>,
    playlists: Vec<Item>,
    tracks: Vec<Track>,
}

/// A plugin's albums, artists and tracks (`library` capability), kept in
/// memory for the session.
#[derive(Clone, Default)]
pub struct PluginLib {
    pub id: String,
    pub name: String,
    pub albums: Vec<Item>,
    pub artists: Vec<Item>,
    pub tracks: Vec<Item>,
    pub playlists: Vec<Item>,
}

/// Which sources the Albums, Artists and Tracks pages show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Local,
    Plugin(String),
}

impl SourceFilter {
    pub fn local(&self) -> bool {
        matches!(self, SourceFilter::All | SourceFilter::Local)
    }

    fn plugin(&self, id: &str) -> bool {
        match self {
            SourceFilter::All => true,
            SourceFilter::Local => false,
            SourceFilter::Plugin(p) => p == id,
        }
    }
}

pub fn browse_arg(id: &str, reference: &str, title: &str) -> String {
    format!("{id}{SEP}{reference}{SEP}{title}")
}

pub fn parse_arg(arg: &str) -> Option<(String, String, String)> {
    let mut it = arg.splitn(3, SEP);
    Some((
        it.next()?.to_string(),
        it.next()?.to_string(),
        it.next().unwrap_or("").to_string(),
    ))
}

pub fn host(ui: &Ui) -> ricercar_core::plugin::PluginHost {
    ui.ctx.plugins.clone()
}

/// Plain-text message for an error (never the plugin's markup).
pub fn error_text(name: &str, e: &PluginError) -> String {
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

/// Plain-text message for an error of plugin `id`.
pub fn error_message(ui: &Ui, id: &str, e: &PluginError) -> String {
    error_text(&name_of(ui, id), e)
}

pub fn name_of(ui: &Ui, id: &str) -> String {
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
    refresh_libraries(ui, &statuses);
    refresh_rows(ui, &statuses);
    crate::plugin_settings::refresh(ui);
    refresh_sections(ui, &statuses);
    refresh_search_scopes(ui, &statuses);
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
                has_settings: !s.settings.is_empty(),
                running: s.state == RunState::Running,
            }
        })
        .collect();
    drop(pv);
    ui.app()
        .set_plugin_updates(rows.iter().filter(|r| r.update).count() as i32);
    ui.app().set_plugin_rows(ModelRc::new(VecModel::from(rows)));
}

/// Home shelves of plugins with `library`: the `home` entries of their
/// `browse.root`, read once per session (first page of each).
pub fn load_home_shelves(ui: &Rc<Ui>) {
    let wanted: Vec<(String, String, Item)> = {
        let pv = ui.plugins.borrow();
        pv.libs
            .iter()
            .flat_map(|l| {
                pv.home
                    .get(&l.id)
                    .into_iter()
                    .flatten()
                    .filter(|i| i.is_browsable())
                    .map(|i| (l.id.clone(), l.name.clone(), i.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    // Forget the shelves of plugins gone or without library.
    ui.plugins.borrow_mut().home_shelves.retain(|s| {
        wanted
            .iter()
            .any(|(id, _, i)| *id == s.id && i.title == s.title)
    });
    show_home_shelves(ui);
    for (id, name, entry) in wanted {
        let key = format!("{id}{SEP}{}", entry.reference);
        {
            let pv = ui.plugins.borrow();
            if pv.loading_home.contains(&key)
                || pv
                    .home_shelves
                    .iter()
                    .any(|s| s.id == id && s.title == entry.title)
            {
                continue;
            }
        }
        ui.plugins.borrow_mut().loading_home.insert(key.clone());
        let h = host(ui);
        std::thread::spawn(move || {
            let r = h.browse_list(&id, &entry.reference, 0, 24);
            post(move |ui| {
                ui.plugins.borrow_mut().loading_home.remove(&key);
                let items = match r {
                    Ok((items, _, _)) => items,
                    Err(e) => {
                        tracing::info!("plugin[{id}] home {}: {e}", entry.title);
                        return;
                    }
                };
                crate::plugin_favs::remember(ui, &id, &items);
                crate::plugin_menu::remember(ui, &id, &items);
                let cards: Vec<AlbumCard> = items
                    .iter()
                    .filter(|i| i.kind != ItemKind::Track && i.is_browsable())
                    .map(|i| album_card(ui, &id, &name, i, true))
                    .collect();
                if cards.is_empty() {
                    return;
                }
                let rows = crate::app::Rows::default();
                set_rows(&rows, cards);
                ui.plugins.borrow_mut().home_shelves.push(HomeShelfRows {
                    id,
                    title: entry.title,
                    source: name,
                    cards: rows,
                });
                show_home_shelves(ui);
            });
        });
    }
}

/// Covers of the plugin Home shelves that never arrived: their requests
/// were dropped by `Loader::cancel_pending` (navigation, scrolling) before
/// they were fetched, and the shelves are built only once. Ask again; the
/// loader skips covers already cached, in flight or known missing.
pub fn request_home_covers(ui: &Ui) {
    use crate::app::Row;
    let keys: Vec<String> = ui
        .plugins
        .borrow()
        .home_shelves
        .iter()
        .flat_map(|s| {
            (0..s.cards.row_count())
                .filter_map(|i| s.cards.row_data(i))
                .filter(|c| c.cover_missing())
                .map(|c| c.ckey().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    for k in keys {
        ui.request_cover(&k, TILE);
    }
}

fn show_home_shelves(ui: &Ui) {
    let pv = ui.plugins.borrow();
    let shelves: Vec<crate::HomeShelf> = pv
        .home_shelves
        .iter()
        .map(|s| crate::HomeShelf {
            title: s.title.clone().into(),
            source: s.source.clone().into(),
            cards: crate::app::model(&s.cards),
        })
        .collect();
    ui.app()
        .set_home_shelves(ModelRc::new(VecModel::from(shelves)));
    drop(pv);
    request_home_covers(ui);
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
        pv.home.retain(|id, _| ready.iter().any(|s| &s.id == id));
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
            let r = h.browse_root_full(&id);
            post(move |ui| {
                ui.plugins.borrow_mut().loading_sections.remove(&id);
                match r {
                    Ok((items, home)) => {
                        let mut pv = ui.plugins.borrow_mut();
                        pv.sections.insert(id.clone(), items);
                        pv.home.insert(id, home.unwrap_or_default());
                        drop(pv);
                        rebuild_nav(ui);
                        load_home_shelves(ui);
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

/// Sidebar: plugins without `library` keep a section of their own; the
/// playlists of those with it join the Playlists list.
fn rebuild_nav(ui: &Ui) {
    let order: Vec<(String, String)> = host(ui)
        .statuses()
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let pv = ui.plugins.borrow();
    let playlists: Vec<PluginNav> = pv
        .libs
        .iter()
        .flat_map(|l| {
            l.playlists
                .iter()
                .filter(|p| p.is_browsable())
                .map(|p| PluginNav {
                    header: false,
                    title: p.title.clone().into(),
                    arg: browse_arg(&l.id, &p.reference, &p.title).into(),
                    kind: "playlist".into(),
                    source: l.name.clone().into(),
                })
        })
        .collect();
    ui.app()
        .set_plugin_playlists(ModelRc::new(VecModel::from(playlists)));
    ui.app().set_plugin_library(!pv.libs.is_empty());
    let mut rows = Vec::new();
    for (id, name) in order {
        let Some(sections) = pv.sections.get(&id) else {
            continue;
        };
        if pv.libs.iter().any(|l| l.id == id) {
            continue;
        }
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
                source: Default::default(),
            });
        }
    }
    ui.app().set_plugin_nav(ModelRc::new(VecModel::from(rows)));
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

pub fn open_url(url: &str) {
    // The headless tours never open a browser.
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some() {
        return;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        crate::sys::xdg_open(url);
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
    card_with(ui, id, it, true)
}

fn card_with(ui: &Ui, id: &str, it: &Item, eager: bool) -> AlbumCard {
    let (cover, ckey) = match art_url(it) {
        Some(url) if eager => (ui.cover(&url, Source::Url(url.clone()), None, TILE), url),
        Some(url) => (
            ui.cover_lazy(&url, Source::Url(url.clone()), None, TILE),
            url,
        ),
        None => (slint::Image::default(), String::new()),
    };
    let hires = it
        .format
        .as_ref()
        .is_some_and(|f| ricercar_core::library::is_hires(f.sample_rate, f.bits));
    let prefix = if it.kind == ItemKind::Album {
        ALBUM_CARD
    } else {
        CARD
    };
    AlbumCard {
        id: format!("{prefix}{}", browse_arg(id, &it.reference, &it.title)).into(),
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
        source: Default::default(),
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
    app.set_br_editable(false);
    app.set_br_can_move(false);
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.browse_serial += 1;
        pv.browse = Some((id.clone(), reference.clone(), 0));
        pv.browse_entries.clear();
        pv.browse_serial
    };
    crate::plugin_menu::browse_opened(ui, &id, &reference, serial);
    check_editable(ui, serial, &id, &reference);
    fetch_page(ui, serial, id, reference, 0, false);
}

fn reload_browse(ui: &Rc<Ui>) {
    if ui.app().get_page() == Page::Browse {
        let arg = ui.app().get_br_arg().to_string();
        load_browse(ui, &arg);
    }
}

/// Read a page of the browse list from `offset`. `replace`: read the list
/// again from the start (as much as was shown, a page at least) and swap it
/// in at once, keeping the page in place (after a playlist edit).
fn fetch_page(ui: &Ui, serial: u64, id: String, reference: String, offset: usize, replace: bool) {
    let limit = if replace {
        ui.models.br_tracks.row_count().clamp(PAGE, 200)
    } else {
        PAGE
    };
    ui.app().set_br_loading(true);
    let h = host(ui);
    std::thread::spawn(move || {
        let r = h.browse_list(&id, &reference, offset, limit);
        post(move |ui| {
            if ui.plugins.borrow().browse_serial != serial {
                return;
            }
            let app = ui.app();
            app.set_br_loading(false);
            match r {
                Ok((items, _, more)) => {
                    crate::plugin_favs::remember(ui, &id, &items);
                    crate::plugin_menu::remember(ui, &id, &items);
                    let cards: Vec<AlbumCard> = items
                        .iter()
                        .filter(|i| i.kind != ItemKind::Track && i.is_browsable())
                        .map(|i| card(ui, &id, i))
                        .collect();
                    let tracks = tracks_of(&id, &items);
                    let entries = items
                        .iter()
                        .filter(|i| i.kind == ItemKind::Track)
                        .map(|i| i.entry_id.clone());
                    if replace {
                        set_rows(&ui.models.br_cards, cards);
                        set_rows(&ui.models.br_tracks, track_rows(ui, &tracks, row_opts(0)));
                        ui.plugins.borrow_mut().browse_entries = entries.collect();
                        ui.st.borrow_mut().lists.insert("browse".into(), tracks);
                    } else {
                        ui.models.br_cards.extend(cards);
                        let rows =
                            track_rows(ui, &tracks, row_opts(ui.models.br_tracks.row_count()));
                        ui.models.br_tracks.extend(rows);
                        ui.plugins.borrow_mut().browse_entries.extend(entries);
                        if let Some(list) = ui.st.borrow_mut().lists.get_mut("browse") {
                            list.extend(tracks);
                        }
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
        fetch_page(ui, serial, id, reference, offset, false);
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
        "artist" => return crate::plugin_menu::card_artist(ui, &id, &reference),
        "fav" => {
            crate::plugin_favs::toggle(ui, &ricercar_core::plugin::plugin_uri(&id, &reference))
        }
        "radio" => return crate::details::item_radio(ui, &id, &reference),
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

// ------------------------------------------------------------ playlist editing

/// Prefix of a plugin playlist among the "Add to playlist" targets (after
/// `pl:`), followed by `<plugin id>SEP<ref>`. Local targets are playlist ids.
pub const PL_TARGET: &str = "plugin-pl\u{1f}";

/// Whether items from `sources` (`None`: a local file, `Some(id)`: a track of
/// plugin `id`) may go to a playlist of `target` (`None`: a local one).
/// Local playlists take anything; a plugin playlist takes the tracks of that
/// same plugin only. No items (a playlist created on its own) fit anywhere.
pub fn target_fits(sources: &[Option<&str>], target: Option<&str>) -> bool {
    match target {
        None => true,
        Some(p) => sources.iter().all(|s| *s == Some(p)),
    }
}

/// An entry of the "Add to playlist" list: a playlist (`key` goes after
/// `pl:`), or the name of a source heading its playlists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: String,
    pub name: String,
    pub header: bool,
}

/// A plugin that edits its playlists: id, name, editable playlists (ref,
/// title).
type EditSource = (String, String, Vec<(String, String)>);

/// The "Add to playlist" list for items from `sources`: local playlists,
/// then the editable playlists of each plugin that takes those items, under
/// the plugin's name. The local ones get a heading only when plugin
/// playlists follow.
pub fn playlist_targets(
    local_name: &str,
    local: &[(i64, String)],
    plugins: &[EditSource],
    sources: &[Option<&str>],
) -> Vec<Target> {
    let groups: Vec<&EditSource> = plugins
        .iter()
        .filter(|(id, _, pls)| !pls.is_empty() && target_fits(sources, Some(id)))
        .collect();
    let mut out = Vec::new();
    if !groups.is_empty() && !local.is_empty() {
        out.push(Target {
            key: String::new(),
            name: local_name.to_string(),
            header: true,
        });
    }
    out.extend(local.iter().map(|(id, name)| Target {
        key: id.to_string(),
        name: name.clone(),
        header: false,
    }));
    for (id, name, pls) in groups {
        out.push(Target {
            key: String::new(),
            name: name.clone(),
            header: true,
        });
        out.extend(pls.iter().map(|(r, title)| Target {
            key: target_key(id, r),
            name: title.clone(),
            header: false,
        }));
    }
    out
}

fn target_key(id: &str, reference: &str) -> String {
    format!("{PL_TARGET}{id}{SEP}{reference}")
}

/// Signed-in plugins declaring `playlist_edit`, with the playlists they mark
/// editable (from their library list).
fn edit_sources(ui: &Ui) -> Vec<EditSource> {
    let pv = ui.plugins.borrow();
    host(ui)
        .statuses()
        .into_iter()
        .filter(|s| s.signed_in() && s.caps.playlist_edit)
        .map(|s| {
            let pls = pv
                .libs
                .iter()
                .find(|l| l.id == s.id)
                .map(|l| {
                    l.playlists
                        .iter()
                        .filter(|p| p.kind == ItemKind::Playlist && p.editable)
                        .map(|p| (p.reference.clone(), p.title.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (s.id, s.name, pls)
        })
        .collect()
}

/// Plugin of a track (`None`: a local file).
fn track_source(tr: &Track) -> Option<String> {
    ricercar_core::plugin::parse_plugin_uri(&tr.path).map(|(id, _)| id)
}

/// Sources of what the menu acts on: `what` 0 nothing (the sidebar's
/// button), 1 the menu's track, 2 the menu's album or plugin card.
fn menu_sources(ui: &Ui, what: i32) -> Vec<Option<String>> {
    let menu = ui.window.global::<crate::TrackMenu>();
    match what {
        1 => crate::views::list_tracks(ui, &menu.get_list())
            .get(menu.get_index().max(0) as usize)
            .map(|tr| vec![track_source(tr)])
            .unwrap_or_default(),
        2 => {
            let id = menu.get_album_id();
            vec![card_target(&id).and_then(|(arg, _)| parse_arg(arg).map(|(p, _, _)| p))]
        }
        _ => Vec::new(),
    }
}

/// Fill the "Add to playlist" list and the new-playlist locations with the
/// playlists and plugins that fit what the menu acts on (see
/// [`menu_sources`]).
pub fn show_playlist_targets(ui: &Ui, what: i32) {
    let sources = menu_sources(ui, what);
    let sources: Vec<Option<&str>> = sources.iter().map(Option::as_deref).collect();
    let local: Vec<(i64, String)> = ui
        .ctx
        .lib
        .playlists()
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let plugins = edit_sources(ui);
    let rows: Vec<crate::PlaylistTarget> =
        playlist_targets(t("Library"), &local, &plugins, &sources)
            .into_iter()
            .map(|t| crate::PlaylistTarget {
                key: t.key.into(),
                name: t.name.into(),
                header: t.header,
            })
            .collect();
    let fitting: Vec<&EditSource> = plugins
        .iter()
        .filter(|(id, _, _)| target_fits(&sources, Some(id)))
        .collect();
    let mut names: Vec<slint::SharedString> = vec![t("Library").into()];
    names.extend(fitting.iter().map(|(_, n, _)| n.as_str().into()));
    ui.plugins.borrow_mut().new_locations = fitting.iter().map(|(id, _, _)| id.clone()).collect();
    let app = ui.app();
    app.set_pl_targets(ModelRc::new(VecModel::from(rows)));
    app.set_new_pl_locations(ModelRc::new(VecModel::from(names)));
    app.set_new_pl_location(0);
}

/// The plugin's playlists as read again after an edit: the sidebar and the
/// "Add to playlist" list follow.
fn apply_playlists(ui: &Ui, id: &str, playlists: Option<Vec<Item>>) {
    let Some(list) = playlists else { return };
    if let Some(l) = ui.plugins.borrow_mut().libs.iter_mut().find(|l| l.id == id) {
        l.playlists = list;
    }
    rebuild_nav(ui);
}

/// The browse page shows this plugin playlist.
fn browsing(ui: &Ui, id: &str, reference: &str) -> bool {
    ui.app().get_page() == Page::Browse
        && ui
            .plugins
            .borrow()
            .browse
            .as_ref()
            .is_some_and(|b| b.0 == id && b.1 == reference)
}

/// Read the open playlist again, in place.
fn refresh_browse(ui: &Ui) {
    let (serial, b) = {
        let pv = ui.plugins.borrow();
        (pv.browse_serial, pv.browse.clone())
    };
    if let Some((id, reference, _)) = b {
        fetch_page(ui, serial, id, reference, 0, true);
    }
}

/// Whether the browse page's list is a playlist the user may edit, and
/// whether its entries can be moved.
fn check_editable(ui: &Ui, serial: u64, id: &str, reference: &str) {
    if !host(ui).status(id).is_some_and(|s| s.caps.playlist_edit) {
        return;
    }
    let (h, id, reference) = (host(ui), id.to_string(), reference.to_string());
    std::thread::spawn(move || {
        let editable = h.playlist_editable(&id, &reference);
        let movable = editable && h.playlist_move_supported(&id);
        post(move |ui| {
            if ui.plugins.borrow().browse_serial == serial {
                ui.app().set_br_editable(editable);
                ui.app().set_br_can_move(movable);
            }
        });
    });
}

/// The playlist on the browse page, when it is editable.
fn edited_playlist(ui: &Ui) -> Option<(String, String)> {
    if !ui.app().get_br_editable() {
        return None;
    }
    ui.plugins
        .borrow()
        .browse
        .as_ref()
        .map(|b| (b.0.clone(), b.1.clone()))
}

/// Create a playlist on the plugin at `location` of the new-playlist
/// dialog (1…); `pending` 1 or 2 then adds the menu's track or album to it.
pub fn create_playlist(ui: &Ui, location: i32, name: &str, pending: i32) {
    let Some(id) = ui
        .plugins
        .borrow()
        .new_locations
        .get((location - 1).max(0) as usize)
        .cloned()
    else {
        return;
    };
    let menu = ui.window.global::<crate::TrackMenu>();
    let track = (pending == 1)
        .then(|| {
            crate::views::list_tracks(ui, &menu.get_list())
                .get(menu.get_index().max(0) as usize)
                .cloned()
        })
        .flatten();
    let album = (pending == 2).then(|| menu.get_album_id().to_string());
    let (h, name) = (host(ui), name.trim().to_string());
    std::thread::spawn(move || {
        let r = h.playlist_create(&id, &name, None, None);
        post(move |ui| match r {
            Ok(ed) => {
                apply_playlists(ui, &id, ed.playlists);
                ui.toast(t("Playlist created"), false);
                let key = target_key(&id, &ed.value.reference);
                if let Some(tr) = track {
                    crate::views::add_to_playlist(ui, &key, std::slice::from_ref(&tr));
                } else if let Some(album) = album {
                    crate::views::album_action(ui, &album, &format!("pl:{key}"));
                }
            }
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

/// Append tracks to a plugin playlist (`target`: `<id>SEP<ref>`). Only
/// tracks of that plugin go there.
pub fn add_to_playlist(ui: &Ui, target: &str, tracks: &[Track]) {
    let Some((id, reference)) = target.split_once(SEP) else {
        return;
    };
    let sources: Vec<Option<String>> = tracks.iter().map(track_source).collect();
    let sources: Vec<Option<&str>> = sources.iter().map(Option::as_deref).collect();
    if tracks.is_empty() || !target_fits(&sources, Some(id)) {
        ui.toast(
            t("Only tracks from the same service can go to this playlist"),
            true,
        );
        return;
    }
    let uris: Vec<String> = tracks.iter().map(|t| t.path.clone()).collect();
    let (h, id, reference) = (host(ui), id.to_string(), reference.to_string());
    std::thread::spawn(move || {
        let mut r = Err(PluginError::NotFound);
        for chunk in uris.chunks(500) {
            r = h.playlist_add(&id, &reference, chunk);
            if r.is_err() {
                break;
            }
        }
        post(move |ui| match r {
            Ok(ed) => {
                apply_playlists(ui, &id, ed.playlists);
                if browsing(ui, &id, &reference) {
                    refresh_browse(ui);
                }
                let name = ui
                    .plugins
                    .borrow()
                    .libs
                    .iter()
                    .find(|l| l.id == id)
                    .and_then(|l| l.playlists.iter().find(|p| p.reference == reference))
                    .map(|p| p.title.clone())
                    .unwrap_or_default();
                ui.toast(format!("+ {name}"), false);
            }
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

/// Remove (`pl-remove`) or move (`pl-up`, `pl-down`) the entry at `index`
/// of the playlist on the browse page.
pub fn entry_action(ui: &Ui, index: usize, action: &str) {
    let Some((id, reference)) = edited_playlist(ui) else {
        return;
    };
    let Some(entry) = ui
        .plugins
        .borrow()
        .browse_entries
        .get(index)
        .cloned()
        .flatten()
    else {
        ui.toast(t("This track cannot be changed in the playlist"), true);
        return;
    };
    let to = match action {
        "pl-up" if index > 0 => Some(index - 1),
        "pl-down" => Some(index + 1),
        "pl-remove" => None,
        _ => return,
    };
    let (h, action) = (host(ui), action.to_string());
    std::thread::spawn(move || {
        let r = match to {
            Some(to) => h.playlist_move(&id, &reference, &entry, to),
            None => h.playlist_remove(&id, &reference, std::slice::from_ref(&entry)),
        };
        let movable = to.is_none() || h.playlist_move_supported(&id);
        post(move |ui| match r {
            Ok(ed) => {
                apply_playlists(ui, &id, ed.playlists);
                if browsing(ui, &id, &reference) {
                    refresh_browse(ui);
                }
                if action == "pl-remove" {
                    ui.toast(t("Removed from the playlist"), false);
                }
            }
            Err(e) => {
                if !movable {
                    ui.app().set_br_can_move(false);
                }
                ui.toast(error_text(&name_of(ui, &id), &e), true);
            }
        });
    });
}

/// Rename the playlist on the browse page.
fn rename_playlist(ui: &Ui, name: &str) {
    let Some((id, reference)) = edited_playlist(ui) else {
        return;
    };
    let (h, name) = (host(ui), name.trim().to_string());
    if name.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let r = h.playlist_rename(&id, &reference, &name);
        post(move |ui| match r {
            Ok(ed) => {
                // The service may adjust the name: show the one it keeps.
                let title = ed
                    .playlists
                    .as_ref()
                    .and_then(|l| l.iter().find(|p| p.reference == reference))
                    .map_or(name, |p| p.title.clone());
                apply_playlists(ui, &id, ed.playlists);
                if !browsing(ui, &id, &reference) {
                    return;
                }
                let arg = browse_arg(&id, &reference, &title);
                let app = ui.app();
                app.set_br_title(title.into());
                app.set_br_arg(arg.clone().into());
                let mut st = ui.st.borrow_mut();
                let pos = st.hist_pos;
                if let Some(h) = st.history.get_mut(pos) {
                    h.1 = arg;
                }
            }
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

/// Delete the playlist on the browse page from the service (the dialog
/// asked first).
fn delete_playlist(ui: &Ui) {
    let Some((id, reference)) = edited_playlist(ui) else {
        return;
    };
    let h = host(ui);
    std::thread::spawn(move || {
        let r = h.playlist_delete(&id, &reference);
        post(move |ui| match r {
            Ok(ed) => {
                let playlists = ed.playlists.or_else(|| {
                    // Not read again: drop it from the list as known.
                    let pv = ui.plugins.borrow();
                    let l = pv.libs.iter().find(|l| l.id == id)?;
                    Some(
                        l.playlists
                            .iter()
                            .filter(|p| p.reference != reference)
                            .cloned()
                            .collect(),
                    )
                });
                apply_playlists(ui, &id, playlists);
                ui.toast(t("Playlist deleted"), false);
                if browsing(ui, &id, &reference) {
                    ui.navigate(Page::Home, "", true);
                }
            }
            Err(e) => ui.toast(error_text(&name_of(ui, &id), &e), true),
        });
    });
}

// ------------------------------------------------------------ search

/// Search scopes: "Everything", "My library", then each signed-in plugin
/// that searches or shares its library. A selected plugin stays selected
/// while it is there.
fn refresh_search_scopes(ui: &Ui, statuses: &[PluginStatus]) {
    let usable: Vec<&PluginStatus> = statuses
        .iter()
        .filter(|s| s.signed_in() && (s.caps.search || s.caps.library))
        .collect();
    let ids: Vec<String> = usable.iter().map(|s| s.id.clone()).collect();
    let mut names: Vec<slint::SharedString> = vec![t("Everything").into(), t("My library").into()];
    names.extend(
        usable
            .iter()
            .map(|s| slint::SharedString::from(s.name.as_str())),
    );
    let app = ui.app();
    let scope = app.get_search_scope();
    let selected = if scope >= 2 {
        let current = ui
            .plugins
            .borrow()
            .scope_ids
            .get(scope as usize - 2)
            .cloned();
        current
            .and_then(|c| ids.iter().position(|i| *i == c))
            .map_or(0, |p| p as i32 + 2)
    } else {
        scope
    };
    ui.plugins.borrow_mut().scope_ids = ids;
    app.set_search_scopes(ModelRc::new(VecModel::from(names)));
    app.set_search_scope(selected);
}

/// Most results kept per plugin and list.
const SEARCH_MAX: usize = 50;

/// Show the local results, then search each signed-in plugin in parallel
/// (plugins without `search` are matched in their library list). Results
/// come in as each plugin answers.
pub fn run_search(ui: &Rc<Ui>, q: &str, local: LocalResults) {
    let words = search_words(q);
    let statuses = host(ui).statuses();
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.search_serial += 1;
        let mut parts = Vec::new();
        // Search page scope: 0 everything, 1 the user's library only,
        // 2… one plugin (its library there and its catalogue).
        let scope = ui.app().get_search_scope();
        let only = (scope >= 2)
            .then(|| pv.scope_ids.get(scope as usize - 2).cloned())
            .flatten();
        let catalogue = scope == 0 || only.is_some();
        let local = if only.is_some() {
            LocalResults::default()
        } else {
            local
        };
        for s in statuses.iter().filter(|s| {
            s.signed_in() && !words.is_empty() && only.as_ref().is_none_or(|o| *o == s.id)
        }) {
            if let Some(lib) = pv.libs.iter().find(|l| l.id == s.id) {
                parts.push(library_matches(lib, &words));
            }
            if s.caps.search && catalogue {
                parts.push(SearchPart {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    pending: true,
                    ..Default::default()
                });
            }
        }
        pv.search = parts;
        pv.search_local = Some(local);
        pv.search_serial
    };
    show_search(ui);
    let pending: Vec<String> = ui
        .plugins
        .borrow()
        .search
        .iter()
        .filter(|p| p.pending)
        .map(|p| p.id.clone())
        .collect();
    for id in pending {
        let (h, q) = (host(ui), q.to_string());
        std::thread::spawn(move || {
            let r = h.search(&id, &q, 0, SEARCH_MAX);
            post(move |ui| {
                {
                    let mut pv = ui.plugins.borrow_mut();
                    if pv.search_serial != serial {
                        return;
                    }
                    let Some(part) = pv.search.iter_mut().find(|p| p.id == id && !p.mine) else {
                        return;
                    };
                    part.pending = false;
                    match r {
                        Ok(groups) => {
                            let items: Vec<Item> = groups.into_iter().flat_map(|g| g.1).collect();
                            crate::plugin_favs::remember(ui, &id, &items);
                            fill_part(part, &items);
                            crate::plugin_menu::remember_in(&mut pv.known, &id, &items);
                        }
                        Err(e) => part.error = Some(error_text(&part.name, &e)),
                    }
                }
                show_search(ui);
            });
        });
    }
}

/// Lowercase words of a query.
fn search_words(q: &str) -> Vec<String> {
    q.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether every word appears in the item's title, artist or album.
fn item_matches(it: &Item, words: &[String]) -> bool {
    let text = [
        Some(it.title.as_str()),
        it.artist.as_deref(),
        it.album_artist.as_deref(),
        it.album.as_deref(),
        it.subtitle.as_deref(),
    ]
    .iter()
    .flatten()
    .map(|s| s.to_lowercase())
    .collect::<Vec<_>>()
    .join(" ");
    words.iter().all(|w| text.contains(w.as_str()))
}

fn library_matches(lib: &PluginLib, words: &[String]) -> SearchPart {
    let pick = |list: &[Item]| -> Vec<Item> {
        list.iter()
            .filter(|i| item_matches(i, words))
            .take(SEARCH_MAX)
            .cloned()
            .collect()
    };
    let mut part = SearchPart {
        id: lib.id.clone(),
        name: lib.name.clone(),
        mine: true,
        artists: pick(&lib.artists),
        albums: pick(&lib.albums),
        playlists: pick(&lib.playlists),
        ..Default::default()
    };
    part.tracks = tracks_of(&lib.id, &pick(&lib.tracks));
    part
}

fn fill_part(part: &mut SearchPart, items: &[Item]) {
    part.artists = items
        .iter()
        .filter(|i| i.kind == ItemKind::Artist)
        .cloned()
        .collect();
    part.albums = items
        .iter()
        .filter(|i| {
            !matches!(
                i.kind,
                ItemKind::Track | ItemKind::Artist | ItemKind::Playlist
            ) && i.is_browsable()
        })
        .cloned()
        .collect();
    part.playlists = items
        .iter()
        .filter(|i| i.kind == ItemKind::Playlist)
        .cloned()
        .collect();
    part.tracks = tracks_of(&part.id, items);
}

/// Fill the search page: the user's library first (local files and
/// playlists, then what plugins hold for them), the catalogues after;
/// within each, tracks alternate between sources so that every source
/// shows near the top.
fn show_search(ui: &Rc<Ui>) {
    let (local, parts) = {
        let pv = ui.plugins.borrow();
        (
            pv.search_local.clone().unwrap_or_default(),
            pv.search.clone(),
        )
    };
    let mut artists: Vec<(String, ArtistCard)> = local
        .artists
        .into_iter()
        .map(|a| (a.name.to_lowercase(), a))
        .collect();
    let mut albums = local.albums;
    let mut playlists = local.playlists;
    let mut sources = vec![local.tracks];
    let mut catalogue = Vec::new();
    let (mut pending, mut errors) = (Vec::new(), Vec::new());
    let (mine, theirs): (Vec<&SearchPart>, Vec<&SearchPart>) = parts.iter().partition(|p| p.mine);
    for p in mine.into_iter().chain(theirs) {
        if p.pending {
            pending.push(p.name.clone());
        }
        errors.extend(p.error.clone());
        let extra: Vec<(String, Item)> = p
            .artists
            .iter()
            .map(|a| (p.name.clone(), a.clone()))
            .collect();
        merge_artist_cards(ui, &mut artists, &extra, true);
        // An item found in both the library and the catalogue shows once.
        let add = |cards: &mut Vec<AlbumCard>, items: &[Item]| {
            for i in items {
                let c = album_card(ui, &p.id, &p.name, i, true);
                if !cards.iter().any(|x| x.id == c.id) {
                    cards.push(c);
                }
            }
        };
        add(&mut playlists, &p.playlists);
        add(&mut albums, &p.albums);
        if p.mine {
            sources.push(p.tracks.clone());
        } else {
            catalogue.push(p.tracks.clone());
        }
    }
    let mut tracks = interleave(sources);
    for t in interleave(catalogue) {
        if !tracks.iter().any(|x| x.path == t.path) {
            tracks.push(t);
        }
    }
    let m = &ui.models;
    set_rows(&m.s_artists, artists.into_iter().map(|(_, c)| c).collect());
    set_rows(&m.s_albums, albums);
    set_rows(&m.s_playlists, playlists);
    set_rows(&m.s_tracks, track_rows(ui, &tracks, row_opts(0)));
    ui.st.borrow_mut().lists.insert("search".into(), tracks);
    let app = ui.app();
    app.set_search_pending(pending.join(", ").into());
    app.set_search_errors(errors.join("\n").into());
}

/// Round-robin over lists, keeping each list's order.
fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let total = lists.iter().map(Vec::len).sum();
    let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        for it in iters.iter_mut() {
            out.extend(it.next());
        }
    }
    out
}

/// Add plugin artists (plugin name, item) to artist cards keyed by
/// lowercase name: a known artist gains the plugin's badge, a new one gets
/// its own card.
pub fn merge_artist_cards(
    ui: &Ui,
    rows: &mut Vec<(String, ArtistCard)>,
    extra: &[(String, Item)],
    eager: bool,
) {
    let mut index: HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (k, _))| (k.clone(), i))
        .collect();
    for (name, it) in extra {
        let k = it.title.to_lowercase();
        match index.get(&k) {
            Some(&i) => {
                let card = &mut rows[i].1;
                if !card.source.split(" · ").any(|s| s == name) {
                    card.source = if card.source.is_empty() {
                        name.clone().into()
                    } else {
                        format!("{} · {name}", card.source).into()
                    };
                }
            }
            None => {
                let (cover, ckey) = match art_url(it) {
                    Some(url) if eager => {
                        (ui.cover(&url, Source::Url(url.clone()), None, TILE), url)
                    }
                    Some(url) => (
                        ui.cover_lazy(&url, Source::Url(url.clone()), None, TILE),
                        url,
                    ),
                    None => (slint::Image::default(), String::new()),
                };
                index.insert(k.clone(), rows.len());
                rows.push((
                    k,
                    ArtistCard {
                        name: it.title.clone().into(),
                        albums: 0,
                        tracks: 0,
                        cover,
                        ckey: ckey.into(),
                        source: name.clone().into(),
                    },
                ));
            }
        }
    }
}

// ------------------------------------------------------------ now playing

/// The album of a playing plugin track: from the plugin's library list,
/// else the first album of that title its search finds.
pub fn open_track_album(ui: &Rc<Ui>, info: &TrackInfo) {
    let Some((id, _)) = ricercar_core::plugin::parse_plugin_uri(&info.uri) else {
        return;
    };
    let Some(album) = info.album.clone().filter(|a| !a.is_empty()) else {
        return;
    };
    let artist = info
        .album_artist
        .clone()
        .or(info.artist.clone())
        .unwrap_or_default();
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let known = ui
        .plugins
        .borrow()
        .libs
        .iter()
        .find(|l| l.id == id)
        .and_then(|l| {
            l.albums
                .iter()
                .find(|a| {
                    same(&a.title, &album) && a.artist.as_deref().is_none_or(|x| same(x, &artist))
                })
                .cloned()
        });
    if let Some(it) = known {
        return open_plugin_album(ui, &id, &it);
    }
    let h = host(ui);
    std::thread::spawn(move || {
        let found = h
            .search(&id, &format!("{album} {artist}"), 0, 20)
            .ok()
            .and_then(|groups| {
                groups
                    .into_iter()
                    .flat_map(|g| g.1)
                    .find(|i| i.kind == ItemKind::Album && same(&i.title, &album))
            });
        if let Some(it) = found {
            post(move |ui| open_plugin_album(ui, &id, &it));
        }
    });
}

pub fn open_plugin_album(ui: &Rc<Ui>, id: &str, it: &Item) {
    let arg = format!("{ALBUM_CARD}{}", browse_arg(id, &it.reference, &it.title));
    ui.navigate(Page::Album, &arg, true);
}

/// The artist of a playing plugin track: the artist page, which finds the
/// artist in the plugins' libraries or searches (`add_artist_albums`).
pub fn open_track_artist(ui: &Rc<Ui>, info: &TrackInfo) {
    if let Some(name) = info
        .artist
        .clone()
        .or(info.album_artist.clone())
        .filter(|a| !a.is_empty())
    {
        ui.navigate(Page::Artist, &name, true);
    }
}

// ------------------------------------------------------------ libraries

/// Plugin names by id.
pub fn names(ui: &Ui) -> HashMap<String, String> {
    host(ui)
        .statuses()
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect()
}

/// Read the library lists of newly signed-in plugins; forget those of
/// plugins signed out or stopped. Pages that show them reload.
fn refresh_libraries(ui: &Rc<Ui>, statuses: &[PluginStatus]) {
    let ready: Vec<&PluginStatus> = statuses
        .iter()
        .filter(|s| s.signed_in() && s.caps.library)
        .collect();
    let dropped = {
        let mut pv = ui.plugins.borrow_mut();
        let before = pv.libs.len();
        pv.libs.retain(|l| ready.iter().any(|s| s.id == l.id));
        pv.libs.len() != before
    };
    if dropped {
        refresh_library_sources(ui);
        reload_library_page(ui);
        rebuild_nav(ui);
        load_home_shelves(ui);
    }
    for s in ready {
        let known = {
            let pv = ui.plugins.borrow();
            pv.libs.iter().any(|l| l.id == s.id) || pv.loading_libs.contains(&s.id)
        };
        if known {
            continue;
        }
        ui.plugins.borrow_mut().loading_libs.insert(s.id.clone());
        let (h, id, name) = (host(ui), s.id.clone(), s.name.clone());
        std::thread::spawn(move || {
            use ricercar_core::plugin::{LIBRARY_MAX, LibraryList};
            let read = |l| h.library_all(&id, l, LIBRARY_MAX);
            let r = (|| {
                Ok::<_, PluginError>(PluginLib {
                    id: id.clone(),
                    name,
                    albums: read(LibraryList::Albums)?,
                    artists: read(LibraryList::Artists)?,
                    tracks: read(LibraryList::Tracks)?,
                    // Optional in the protocol.
                    playlists: read(LibraryList::Playlists).unwrap_or_else(|e| {
                        tracing::debug!("plugin[{id}] library.playlists: {e}");
                        Vec::new()
                    }),
                })
            })();
            post(move |ui| {
                ui.plugins.borrow_mut().loading_libs.remove(&id);
                match r {
                    Ok(lib) => {
                        tracing::info!(
                            "plugin[{id}] library: {} albums, {} artists, {} tracks",
                            lib.albums.len(),
                            lib.artists.len(),
                            lib.tracks.len()
                        );
                        for list in [&lib.albums, &lib.artists, &lib.tracks] {
                            crate::plugin_favs::remember(ui, &id, list);
                        }
                        let order: Vec<String> =
                            host(ui).statuses().into_iter().map(|s| s.id).collect();
                        let mut pv = ui.plugins.borrow_mut();
                        pv.libs.retain(|l| l.id != id);
                        pv.libs.push(lib);
                        pv.libs
                            .sort_by_key(|l| order.iter().position(|o| *o == l.id));
                        drop(pv);
                        refresh_library_sources(ui);
                        reload_library_page(ui);
                        rebuild_nav(ui);
                        load_home_shelves(ui);
                    }
                    Err(e) => tracing::warn!("plugin[{id}] library: {e}"),
                }
            });
        });
    }
}

fn reload_library_page(ui: &Rc<Ui>) {
    let page = ui.app().get_page();
    if matches!(
        page,
        Page::Albums | Page::Artists | Page::Tracks | Page::Artist | Page::Search
    ) {
        ui.reload_page();
    }
}

/// "All", "Library", then each plugin that shares a library; the selection
/// follows its plugin when the list changes.
fn refresh_library_sources(ui: &Ui) {
    let app = ui.app();
    let current = source_filter(ui);
    let pv = ui.plugins.borrow();
    let mut names: Vec<slint::SharedString> = vec![t("All").into(), t("Library").into()];
    names.extend(
        pv.libs
            .iter()
            .map(|l| slint::SharedString::from(l.name.as_str())),
    );
    let selected = match &current {
        SourceFilter::All => 0,
        SourceFilter::Local => 1,
        SourceFilter::Plugin(id) => pv
            .libs
            .iter()
            .position(|l| &l.id == id)
            .map(|p| p as i32 + 2)
            .unwrap_or(0),
    };
    drop(pv);
    app.set_library_sources(ModelRc::new(VecModel::from(names)));
    app.set_library_source(selected);
}

pub fn source_filter(ui: &Ui) -> SourceFilter {
    let i = ui.app().get_library_source();
    let pv = ui.plugins.borrow();
    match i {
        1 => SourceFilter::Local,
        i if i >= 2 => pv
            .libs
            .get(i as usize - 2)
            .map(|l| SourceFilter::Plugin(l.id.clone()))
            .unwrap_or(SourceFilter::All),
        _ => SourceFilter::All,
    }
}

/// (plugin id, plugin name, item) of the selected plugins' lists.
fn picked(
    ui: &Ui,
    f: &SourceFilter,
    pick: impl Fn(&PluginLib) -> &Vec<Item>,
) -> Vec<(String, String, Item)> {
    ui.plugins
        .borrow()
        .libs
        .iter()
        .filter(|l| f.plugin(&l.id))
        .flat_map(|l| {
            pick(l)
                .iter()
                .map(|i| (l.id.clone(), l.name.clone(), i.clone()))
        })
        .collect()
}

pub fn plugin_albums(ui: &Ui, f: &SourceFilter) -> Vec<(String, String, Item)> {
    picked(ui, f, |l| &l.albums)
}

pub fn plugin_artists(ui: &Ui, f: &SourceFilter) -> Vec<(String, String, Item)> {
    picked(ui, f, |l| &l.artists)
}

/// Plugin tracks as library tracks (plugin:// paths).
pub fn plugin_tracks(ui: &Ui, f: &SourceFilter) -> Vec<Track> {
    picked(ui, f, |l| &l.tracks)
        .into_iter()
        .filter(|(_, _, i)| i.kind == ItemKind::Track)
        .map(|(id, _, i)| Track::from_info(&i.to_track_info(&id)))
        .collect()
}

/// Album tile of a plugin album, marked with its plugin's name.
pub fn album_card(ui: &Ui, id: &str, name: &str, it: &Item, eager: bool) -> AlbumCard {
    let mut c = card_with(ui, id, it, eager);
    c.source = name.into();
    if let Some(a) = it.artist.clone().filter(|a| !a.is_empty()) {
        c.artist = a.into();
    }
    if let Some(y) = it.year {
        c.year = y.to_string().into();
    }
    c
}

pub fn art_url(it: &Item) -> Option<String> {
    it.art
        .clone()
        .filter(|a| a.starts_with("https://") || a.starts_with("http://"))
}

// ------------------------------------------------------------ album & artist pages

/// A plugin album on the album page: header from its item (the library
/// list, else item.get), tracks from browse.list.
pub fn load_album_page(ui: &Rc<Ui>, card_id: &str, arg: &str) {
    let Some((id, reference, title)) = parse_arg(arg) else {
        return;
    };
    let app = ui.app();
    let name = name_of(ui, &id);
    let known = ui
        .plugins
        .borrow()
        .libs
        .iter()
        .find(|l| l.id == id)
        .and_then(|l| l.albums.iter().find(|a| a.reference == reference).cloned())
        .or_else(|| crate::plugin_menu::find_item(ui, &id, &reference));
    app.set_al_id(card_id.into());
    app.set_al_plugin(true);
    app.set_al_title(title.into());
    app.set_al_artist("".into());
    app.set_al_year("".into());
    app.set_al_genre("".into());
    app.set_al_count(0);
    app.set_al_duration("".into());
    app.set_al_quality("".into());
    app.set_al_hires(false);
    app.set_al_dac_unsupported(false);
    app.set_al_fav(false);
    app.set_al_path(name.into());
    app.set_al_artist_arg("".into());
    app.set_al_label_arg("".into());
    app.set_al_cover(slint::Image::default());
    ui.plugins.borrow_mut().album_art = None;
    set_rows(&ui.models.al_tracks, Vec::new());
    set_rows(&ui.models.al_more, Vec::new());
    ui.st.borrow_mut().lists.insert("album".into(), Vec::new());
    if let Some(item) = &known {
        album_header(ui, &id, item);
    }
    crate::plugin_favs::show_album(ui, &id, &reference);
    let serial = {
        let mut pv = ui.plugins.borrow_mut();
        pv.browse_serial += 1;
        pv.browse_serial
    };
    let h = host(ui);
    let card_id = card_id.to_string();
    std::thread::spawn(move || {
        let head = if known.is_none() {
            h.item_get(&id, &reference).ok()
        } else {
            None
        };
        let mut items = Vec::new();
        let mut err = None;
        loop {
            match h.browse_list(&id, &reference, items.len(), 200) {
                Ok((page, _, more)) => {
                    let n = page.len();
                    items.extend(page);
                    if !more || n == 0 || items.len() >= 2000 {
                        break;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        post(move |ui| {
            if ui.plugins.borrow().browse_serial != serial
                || ui.app().get_al_id() != card_id.as_str()
            {
                return;
            }
            if let Some(h) = &head {
                crate::plugin_menu::remember(ui, &id, std::slice::from_ref(h));
                album_header(ui, &id, h);
                crate::plugin_favs::remember(ui, &id, std::slice::from_ref(h));
            }
            crate::plugin_favs::remember(ui, &id, &items);
            crate::plugin_menu::remember(ui, &id, &items);
            crate::plugin_favs::show_album(ui, &id, &reference);
            if let Some(e) = err {
                ui.toast(error_text(&name_of(ui, &id), &e), true);
            }
            let tracks = tracks_of(&id, &items);
            let app = ui.app();
            app.set_al_count(tracks.len() as i32);
            let total: u64 = tracks.iter().map(|t| t.duration_ms).sum();
            if total > 0 {
                app.set_al_duration(crate::text::long_duration(total).into());
            }
            app.set_al_dac_unsupported(crate::extras::active_caps(ui).is_some_and(|c| {
                tracks
                    .iter()
                    .filter_map(|t| t.sample_rate)
                    .any(|r| !c.supports_rate(r))
            }));
            let o = RowOpts {
                cover: false,
                eager: true,
                track_numbers: true,
                discs: true,
                album_artist: app.get_al_artist().to_string().into(),
                start: 0,
            };
            set_rows(&ui.models.al_tracks, track_rows(ui, &tracks, o));
            ui.st.borrow_mut().lists.insert("album".into(), tracks);
            // Details come after the tracks, never before the page.
            crate::details::load_album(ui, &card_id, &id, &reference);
        });
    });
}

fn album_header(ui: &Ui, id: &str, it: &Item) {
    let app = ui.app();
    if let Some(a) = it.artist.clone().or(it.album_artist.clone()) {
        app.set_al_artist(a.into());
    }
    let (artist, label) = crate::plugin_menu::album_links(id, it);
    app.set_al_artist_arg(artist.into());
    app.set_al_label_arg(label.into());
    app.set_al_year(it.year.map(|y| y.to_string()).unwrap_or_default().into());
    app.set_al_genre(it.genre.clone().unwrap_or_default().into());
    if let Some(f) = &it.format {
        let codec = f.codec.clone().map(|c| c.to_uppercase());
        let q = crate::text::quality(f.sample_rate, f.bits, codec.as_deref(), None);
        app.set_al_quality(
            match (codec, q.is_empty()) {
                (Some(c), false) => format!("{c} {q}"),
                (Some(c), true) => c,
                (None, _) => q,
            }
            .into(),
        );
        app.set_al_hires(ricercar_core::library::is_hires(f.sample_rate, f.bits));
    }
    if let Some(url) = art_url(it) {
        app.set_al_cover(ui.cover(&url, Source::Url(url.clone()), None, crate::app::LARGE));
        ui.plugins.borrow_mut().album_art = Some(url);
    }
}

/// On an artist page, the albums plugins have under the same name.
pub fn add_artist_albums(ui: &Rc<Ui>, name: &str) {
    crate::plugin_favs::reset_artist(ui);
    let refs: Vec<(String, String, String)> = ui
        .plugins
        .borrow()
        .libs
        .iter()
        .flat_map(|l| {
            l.artists
                .iter()
                .filter(|a| {
                    a.title.eq_ignore_ascii_case(name)
                        || a.title.to_lowercase() == name.to_lowercase()
                })
                .map(|a| (l.id.clone(), l.name.clone(), a.reference.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut refs = refs;
    for p in &ui.plugins.borrow().search {
        for a in p
            .artists
            .iter()
            .filter(|a| a.title.to_lowercase() == name.to_lowercase())
        {
            if !refs.iter().any(|(i, _, r)| *i == p.id && *r == a.reference) {
                refs.push((p.id.clone(), p.name.clone(), a.reference.clone()));
            }
        }
    }
    // Plugins that did not list this artist: ask their search.
    let searched: Vec<(String, String)> = host(ui)
        .statuses()
        .into_iter()
        .filter(|s| s.signed_in() && s.caps.search && !refs.iter().any(|(i, _, _)| *i == s.id))
        .map(|s| (s.id, s.name))
        .collect();
    for (id, pname, reference) in refs {
        artist_albums(ui, id, pname, name.to_string(), Some(reference));
    }
    for (id, pname) in searched {
        artist_albums(ui, id, pname, name.to_string(), None);
    }
}

/// Add a plugin artist's albums to the artist page; without a ref, find
/// the artist first with the plugin's search (same name, any case).
fn artist_albums(ui: &Ui, id: String, pname: String, name: String, reference: Option<String>) {
    let h = host(ui);
    std::thread::spawn(move || {
        // Found by the search: the artist item, for its favourite state.
        let mut found: Option<Item> = None;
        let reference = reference.or_else(|| {
            let wanted = name.trim().to_lowercase();
            found = h.search(&id, &name, 0, 20).ok().and_then(|groups| {
                groups.into_iter().flat_map(|g| g.1).find(|i| {
                    i.kind == ItemKind::Artist
                        && i.is_browsable()
                        && i.title.trim().to_lowercase() == wanted
                })
            });
            found.as_ref().map(|i| i.reference.clone())
        });
        let Some(reference) = reference else { return };
        let r = h.browse_list(&id, &reference, 0, 200);
        post(move |ui| {
            if ui.app().get_ar_name() != name.as_str() {
                return;
            }
            if let Some(it) = &found {
                crate::plugin_favs::remember(ui, &id, std::slice::from_ref(it));
            }
            crate::plugin_favs::offer_artist(ui, &id, &reference);
            crate::details::artist_found(ui, &id, &reference, &name);
            let Ok((items, _, _)) = r else { return };
            crate::plugin_menu::remember(ui, &id, &items);
            let cards: Vec<AlbumCard> = items
                .iter()
                .filter(|i| i.kind == ItemKind::Album)
                .map(|i| album_card(ui, &id, &pname, i, true))
                .collect();
            if cards.is_empty() {
                return;
            }
            let app = ui.app();
            app.set_ar_albums(app.get_ar_albums() + cards.len() as i32);
            let counts: Option<Vec<u32>> = items
                .iter()
                .filter(|i| i.kind == ItemKind::Album)
                .map(|i| i.track_count)
                .collect();
            let known = app.get_ar_tracks();
            app.set_ar_tracks(match counts {
                Some(c) if known >= 0 => known + c.iter().sum::<u32>() as i32,
                _ => -1,
            });
            if app.get_ar_cover().size().width == 0
                && let Some(url) = items.iter().find_map(art_url)
            {
                app.set_ar_cover(ui.cover(&url, Source::Url(url.clone()), None, crate::app::LARGE));
            }
            ui.models.ar_own.extend(cards);
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

/// At startup, when plugins were installed from the catalogue: read it
/// once so the Plugins entry can show how many have an update.
pub fn check_updates(ui: &Rc<Ui>) {
    let cfg = ui.ctx.config.read().unwrap();
    if !cfg.online.plugin_catalog || !cfg.plugins.iter().any(|p| p.version.is_some()) {
        return;
    }
    drop(cfg);
    let (url, local) = index_source(ui);
    std::thread::spawn(move || {
        let r = catalog::fetch_index(&url, local);
        post(move |ui| match r {
            Ok(list) => {
                let mut pv = ui.plugins.borrow_mut();
                if pv.catalog.is_empty() {
                    pv.catalog = list;
                }
                pv.rev = None;
                drop(pv);
                poll(ui);
            }
            Err(e) => tracing::info!("plugin update check: {e}"),
        });
    });
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
            tf("Update {name}?", &[("name", &e.name)])
        } else {
            tf("Install {name}?", &[("name", &e.name)])
        }
        .into(),
    );
    let mut body =
        crate::text::install_body(&e.name, &e.version, &e.author, &host_name, &e.repository);
    // An update from somewhere else than the installed binary: say so first.
    if update {
        let installed = ui
            .ctx
            .config
            .read()
            .unwrap()
            .plugins
            .iter()
            .find(|p| p.id == e.id)
            .cloned();
        if let Some(installed) = installed
            && catalog::update_info(&installed, &e).is_some_and(|u| u.host_changed)
        {
            let old = installed.host.unwrap_or_default();
            body = format!("{}\n\n{body}", crate::text::host_changed(&old, &host_name));
        }
    }
    app.set_install_body(body.into());
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
                            // An update keeps the user's settings.
                            Some(p) => {
                                let settings = std::mem::take(&mut p.settings);
                                *p = cfg.clone();
                                p.settings = settings;
                            }
                            None => c.plugins.push(cfg.clone()),
                        });
                    ui.toast(tf("{name} installed", &[("name", &e.name)]), false);
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
    ui.toast(tf("{name} removed", &[("name", &name_of(ui, id))]), false);
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
    app.on_ar_toggle_fav(|| with_ui(|ui| crate::plugin_favs::toggle_artist(ui)));
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
    app.on_br_playlist_rename(|name| with_ui(|ui| rename_playlist(ui, &name)));
    app.on_br_playlist_delete(|| with_ui(|ui| delete_playlist(ui)));
    app.on_playlist_targets(|what| with_ui(|ui| show_playlist_targets(ui, what)));
    app.on_plugin_playlist_create(|location, name, pending| {
        with_ui(|ui| create_playlist(ui, location, &name, pending))
    });
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
    app.on_library_source_changed(|| with_ui(|ui| ui.reload_page()));
    app.on_search_scope_changed(|| {
        with_ui(|ui| {
            let q = ui.app().get_search_text().to_string();
            crate::views::run_search(ui, &q);
        })
    });
    crate::plugin_settings::wire(ui);
    push_output(ui);
    poll(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_every_word_and_alternates_sources() {
        let it = Item {
            title: "Blue Hour".into(),
            artist: Some("Demo Ensemble".into()),
            album: Some("Night Studies".into()),
            ..Default::default()
        };
        assert!(item_matches(&it, &search_words("blue ENSEMBLE")));
        assert!(item_matches(&it, &search_words("night")));
        assert!(!item_matches(&it, &search_words("blue red")));
        assert_eq!(
            interleave(vec![vec![1, 2, 3], vec![], vec![10, 20]]),
            [1, 10, 2, 20, 3]
        );
    }

    #[test]
    fn playlist_targets_follow_the_source_rule() {
        // Local playlists take anything; a plugin's only its own tracks.
        assert!(target_fits(&[None, Some("a")], None));
        assert!(target_fits(&[Some("a"), Some("a")], Some("a")));
        assert!(!target_fits(&[Some("b")], Some("a")));
        assert!(!target_fits(&[None], Some("a")));
        // A mixed selection only fits local playlists.
        assert!(!target_fits(&[Some("a"), None], Some("a")));
        assert!(!target_fits(&[Some("a"), Some("b")], Some("b")));
        // Nothing to add (a playlist created on its own): anywhere.
        assert!(target_fits(&[], Some("a")));

        let local = [(1, "Late night".to_string())];
        let pls = |t: &str| vec![(format!("playlist/{t}"), t.to_string())];
        let plugins = [
            ("a".to_string(), "Service A".to_string(), pls("Mix")),
            ("b".to_string(), "Service B".to_string(), pls("Picks")),
            ("c".to_string(), "Service C".to_string(), Vec::new()),
        ];
        let view = |sources: &[Option<&str>]| -> Vec<(String, bool)> {
            playlist_targets("Library", &local, &plugins, sources)
                .into_iter()
                .map(|t| (t.name, t.header))
                .collect()
        };
        let row = |n: &str, h: bool| (n.to_string(), h);
        // A local track: local playlists only, no headings.
        assert_eq!(view(&[None]), [row("Late night", false)]);
        // A track of plugin a: local ones, then a's under its name.
        assert_eq!(
            view(&[Some("a")]),
            [
                row("Library", true),
                row("Late night", false),
                row("Service A", true),
                row("Mix", false)
            ]
        );
        assert_eq!(view(&[Some("a"), None]), [row("Late night", false)]);
        let keys = playlist_targets("Library", &local, &plugins, &[Some("b")]);
        assert_eq!(keys[1].key, "1");
        assert_eq!(keys[3].key, target_key("b", "playlist/Picks"));
        assert_eq!(
            keys[3].key.strip_prefix(PL_TARGET),
            Some("b\u{1f}playlist/Picks")
        );
        // No local playlist: no "Library" heading either.
        let only = playlist_targets("Library", &[], &plugins, &[Some("a")]);
        assert_eq!(only.len(), 2);
        assert!(only[0].header && only[0].name == "Service A");
    }

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
