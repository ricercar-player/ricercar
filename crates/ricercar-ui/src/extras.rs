//! Radio, settings and integrations.

use std::rc::Rc;

use ricercar_core::config::{ReplayGain, Theme as ThemeCfg};
use ricercar_core::{PlayContext, TrackInfo};
use ricercar_online::radio::{RadioBrowser, Station, StationQuery};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{THUMB, Ui, post, with_ui};
use crate::images::Source;
use crate::text::t;
use crate::{DeviceRow, StationRow};

pub const ACCENTS: [&str; 7] = [
    "#d4a35a", "#e0785a", "#d66a9c", "#8b7cf6", "#4f9cf0", "#3fb8a8", "#7cc46a",
];

#[derive(Default)]
pub struct Extras {
    results: Vec<Station>,
    favs: Vec<Station>,
    radio: Option<RadioBrowser>,
    radio_loaded: bool,
    lastfm_token: Option<String>,
    /// Latest radio query: older answers arriving late are dropped.
    radio_serial: u64,
    /// Pending debounced edits (radio query, settings text, LB token).
    debounce: std::collections::HashMap<&'static str, u64>,
    stations: Option<Rc<VecModel<StationRow>>>,
    fav_model: Option<Rc<VecModel<StationRow>>>,
}

fn luminance(c: [f64; 3]) -> f64 {
    let lin = |v: f64| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// An accent usable as text on the theme background (WCAG 4.5:1): lifted
/// toward white on dark, pushed toward black on light, hue preserved.
pub fn readable_accent(rgb: [u8; 3], dark: bool) -> slint::Color {
    let bg = if dark {
        [0x0f, 0x0f, 0x11]
    } else {
        [0xf6, 0xf5, 0xf2]
    };
    let bg = bg.map(|v| v as f64 / 255.0);
    let target = if dark { 1.0 } else { 0.0 };
    let mut c = rgb.map(|v| v as f64 / 255.0);
    for _ in 0..40 {
        if contrast(c, bg) >= 4.6 {
            break;
        }
        c = c.map(|v| v + (target - v) * 0.08);
    }
    let [r, g, b] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    slint::Color::from_rgb_u8(r, g, b)
}

pub fn apply_cover_accent(ui: &Ui) {
    let dark = ui.ctx.config.read().unwrap().ui.theme == ThemeCfg::Dark;
    let c = match ui.st.borrow().cover_rgb {
        Some(rgb) => readable_accent(rgb, dark),
        None => slint::Color::from_argb_u8(0, 0, 0, 0),
    };
    ui.window.global::<crate::Theme>().set_cover_accent(c);
}

fn parse_hex(s: &str) -> Option<slint::Color> {
    let s = s.trim_start_matches('#');
    let v = u32::from_str_radix(s, 16).ok()?;
    (s.len() == 6).then(|| slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

// ------------------------------------------------------------------ radio

fn favs_path() -> std::path::PathBuf {
    ricercar_core::config::data_dir().join("radio.json")
}

fn station_row(ui: &Ui, s: &Station, fav: bool) -> StationRow {
    let key = if s.favicon.is_empty() {
        String::new()
    } else {
        s.favicon.clone()
    };
    let cover = if key.is_empty() {
        Default::default()
    } else {
        ui.cover(&key, Source::Url(key.clone()), None, THUMB)
    };
    let mut sub = vec![];
    if !s.country.is_empty() {
        sub.push(s.country.clone());
    }
    if !s.codec.is_empty() {
        sub.push(if s.bitrate > 0 {
            format!("{} {} kbit/s", s.codec, s.bitrate)
        } else {
            s.codec.clone()
        });
    }
    StationRow {
        uuid: s.uuid.clone().into(),
        name: s.name.trim().to_string().into(),
        sub: sub.join(" · ").into(),
        tags: s
            .tags
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
        cover,
        ckey: key.into(),
        fav,
    }
}

fn show_stations(ui: &Ui) {
    let (results, favs) = {
        let ex = ui.extras.borrow();
        (ex.results.clone(), ex.favs.clone())
    };
    let fav_ids: std::collections::HashSet<&str> = favs.iter().map(|s| s.uuid.as_str()).collect();
    let rows: Vec<_> = results
        .iter()
        .map(|s| station_row(ui, s, fav_ids.contains(s.uuid.as_str())))
        .collect();
    let frows: Vec<_> = favs.iter().map(|s| station_row(ui, s, true)).collect();
    let app = ui.app();
    let m = Rc::new(VecModel::from(rows));
    let fm = Rc::new(VecModel::from(frows));
    app.set_stations(ModelRc::from(m.clone()));
    app.set_radio_favs(ModelRc::from(fm.clone()));
    let mut ex = ui.extras.borrow_mut();
    ex.stations = Some(m);
    ex.fav_model = Some(fm);
}

pub fn refill_station_covers(ui: &Ui) {
    let ex = ui.extras.borrow();
    let loader = ui.loader.borrow();
    for m in [&ex.stations, &ex.fav_model].into_iter().flatten() {
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
}

/// Run `f` once typing has paused for `ms` (per `key`).
fn debounce(ui: &Rc<Ui>, key: &'static str, ms: u64, f: impl FnOnce(&Rc<Ui>) + 'static) {
    let serial = {
        let mut ex = ui.extras.borrow_mut();
        let s = ex.debounce.entry(key).or_insert(0);
        *s += 1;
        *s
    };
    slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
        with_ui(|ui| {
            if ui.extras.borrow().debounce.get(key) == Some(&serial) {
                f(ui);
            }
        })
    });
}

fn radio_text_search(ui: &Rc<Ui>, q: &str) {
    let q = q.trim().to_string();
    if q.is_empty() {
        radio_query(ui, StationQuery::default(), t("Top stations").into());
    } else {
        let query = StationQuery {
            name: Some(q.clone()),
            limit: 80,
            ..Default::default()
        };
        radio_query(ui, query, format!("{} · {q}", t("Stations")));
    }
}

fn radio_query(ui: &Rc<Ui>, q: StationQuery, label: String) {
    ui.app().set_radio_status(t("Searching…").into());
    let serial = {
        let mut ex = ui.extras.borrow_mut();
        ex.radio_serial += 1;
        ex.radio_serial
    };
    let radio = ui.extras.borrow().radio.clone();
    std::thread::spawn(move || {
        let radio = radio.unwrap_or_else(RadioBrowser::discover);
        let res = if q.name.is_none() && q.tag.is_none() {
            radio.top_clicked(60)
        } else {
            radio.search(&q)
        };
        post(move |ui| {
            ui.extras.borrow_mut().radio = Some(radio);
            if ui.extras.borrow().radio_serial != serial {
                return;
            }
            let app = ui.app();
            match res {
                Ok(list) => {
                    app.set_radio_status(
                        if list.is_empty() {
                            t("No station found").to_string()
                        } else {
                            label
                        }
                        .into(),
                    );
                    ui.extras.borrow_mut().results = list;
                    show_stations(ui);
                }
                Err(e) => {
                    tracing::warn!("radio: {e}");
                    app.set_radio_status(t("Radio Browser is unreachable").into());
                }
            }
        });
    });
}

pub fn load_radio(ui: &Rc<Ui>) {
    if !ui.ctx.config.read().unwrap().online.radio {
        return;
    }
    if ui.extras.borrow().radio_loaded {
        show_stations(ui);
        return;
    }
    ui.extras.borrow_mut().radio_loaded = true;
    let favs: Vec<Station> = std::fs::read(favs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    ui.extras.borrow_mut().favs = favs;
    let tags = [
        "jazz",
        "classical",
        "ambient",
        "electronic",
        "rock",
        "chillout",
        "news",
        "lounge",
        "baroque",
        "soul",
        "funk",
        "hiphop",
        "world",
        "80s",
    ];
    let model: Vec<SharedString> = tags.iter().map(|s| (*s).into()).collect();
    ui.app().set_radio_tags(ModelRc::new(VecModel::from(model)));
    show_stations(ui);
    radio_query(ui, StationQuery::default(), t("Top stations").into());
}

fn play_station(ui: &Rc<Ui>, s: &Station) {
    let info = TrackInfo {
        uri: s.url_resolved.clone(),
        title: s.name.trim().to_string(),
        artist: Some(
            [
                s.country.clone(),
                s.tags.first().cloned().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        ),
        cover: (!s.favicon.is_empty()).then(|| s.favicon.clone()),
        codec: (!s.codec.is_empty()).then(|| s.codec.clone()),
        live: true,
        ..Default::default()
    };
    ui.ctx.ctl.play_tracks(vec![info], 0, PlayContext::Radio);
    let radio = ui.extras.borrow().radio.clone();
    let uuid = s.uuid.clone();
    std::thread::spawn(move || {
        // Radio Browser asks clients to report plays.
        if let Some(r) = radio {
            let _ = r.count_click(&uuid);
        }
    });
}

fn save_favs(ui: &Ui) {
    let favs = ui.extras.borrow().favs.clone();
    if let Ok(json) = serde_json::to_vec_pretty(&favs) {
        let _ = ricercar_core::config::write_atomic(&favs_path(), &json);
    }
}

// ------------------------------------------------------------------ settings

pub fn load_settings(ui: &Rc<Ui>) {
    let cfg = ui.ctx.config.read().unwrap().clone();
    let app = ui.app();
    refresh_devices(ui);
    app.set_replaygain(match cfg.audio.replaygain {
        ReplayGain::Off => 0,
        ReplayGain::Track => 1,
        ReplayGain::Album => 2,
        ReplayGain::Auto => 3,
    });
    app.set_preamp(cfg.audio.preamp_db);
    app.set_restore_session(cfg.audio.restore_session);
    app.set_watch_library(cfg.library.watch);
    app.set_renderer_name(cfg.network.name.clone().into());
    app.set_renderer_enabled(cfg.network.renderer);
    app.set_server_enabled(cfg.network.media_server);
    app.set_network_status(
        match (cfg.network.renderer, ui.ctx.renderer_port) {
            (true, Some(port)) => format!(
                "{} “{}” · {} {port}",
                t("Visible on the network as"),
                cfg.network.name,
                t("port")
            ),
            _ => t("Renderer is off").into(),
        }
        .into(),
    );
    app.set_lb_token(cfg.scrobble.listenbrainz_token.clone().into());
    if cfg.scrobble.listenbrainz_token.is_empty() {
        app.set_lb_status(t("Not connected").into());
    } else if app.get_lb_status().is_empty() {
        check_listenbrainz(ui, cfg.scrobble.listenbrainz_token.clone());
    }
    app.set_lastfm_key(cfg.scrobble.lastfm_api_key.clone().into());
    app.set_lastfm_secret(cfg.scrobble.lastfm_secret.clone().into());
    app.set_lastfm_status(
        if cfg.scrobble.lastfm_session.is_empty() {
            String::new()
        } else {
            format!("{} {}", t("Connected as"), cfg.scrobble.lastfm_user)
        }
        .into(),
    );
    app.set_lyrics_online(cfg.online.lyrics);
    app.set_covers_online(cfg.online.cover_art);
    app.set_notifications(cfg.ui.notifications);
    app.set_close_to_tray(cfg.ui.close_to_tray);
    app.set_theme_dark(cfg.ui.theme == ThemeCfg::Dark);
    app.set_adaptive_colors(cfg.ui.adaptive_colors);
    app.set_accent_choice(
        ACCENTS
            .iter()
            .position(|a| a.eq_ignore_ascii_case(&cfg.ui.accent))
            .unwrap_or(0) as i32,
    );
    refresh_library_rows(ui);
}

pub fn refresh_library_rows(ui: &Ui) {
    let roots: Vec<SharedString> = ui
        .ctx
        .config
        .read()
        .unwrap()
        .library
        .roots
        .iter()
        .map(|r| r.display().to_string().into())
        .collect();
    ui.app().set_roots(ModelRc::new(VecModel::from(roots)));
}

fn refresh_devices(ui: &Ui) {
    let current = ui.ctx.ctl.device_name();
    let mut rows: Vec<DeviceRow> = ricercar_audio::device::list_devices()
        .into_iter()
        .filter(|d| d.kind != ricercar_audio::DeviceKind::Null)
        .map(|d| DeviceRow {
            hardware: d.kind == ricercar_audio::DeviceKind::Hardware,
            shared: d.kind == ricercar_audio::DeviceKind::Virtual,
            selected: d.name == current,
            desc: d
                .description
                .trim_end_matches(" (not bit-perfect)")
                .to_string()
                .into(),
            name: d.name.into(),
        })
        .collect();
    if !rows.iter().any(|r| r.selected) {
        let null = current == "null";
        rows.push(DeviceRow {
            desc: if null {
                t("Null sink").into()
            } else {
                current.clone()
            }
            .into(),
            name: if null {
                t("Discards audio (testing)").into()
            } else {
                current
            }
            .into(),
            hardware: false,
            shared: false,
            selected: true,
        });
    }
    ui.app().set_devices(ModelRc::new(VecModel::from(rows)));
}

/// Apply look & feel from the config to the Theme global.
pub fn apply_theme(ui: &Ui) {
    let cfg = ui.ctx.config.read().unwrap().ui.clone();
    let theme = ui.window.global::<crate::Theme>();
    theme.set_dark(cfg.theme == ThemeCfg::Dark);
    theme.set_adaptive(cfg.adaptive_colors);
    let dark = cfg.theme == ThemeCfg::Dark;
    if let Some(c) = parse_hex(&cfg.accent) {
        theme.set_base_accent(readable_accent([c.red(), c.green(), c.blue()], dark));
    }
    apply_cover_accent(ui);
    ui.app().set_theme_dark(cfg.theme == ThemeCfg::Dark);
}

fn settings_changed(ui: &Rc<Ui>) {
    let app = ui.app();
    let rg = match app.get_replaygain() {
        1 => ReplayGain::Track,
        2 => ReplayGain::Album,
        3 => ReplayGain::Auto,
        _ => ReplayGain::Off,
    };
    let preamp = (app.get_preamp() * 10.0).round() / 10.0;
    let accent = ACCENTS[(app.get_accent_choice().max(0) as usize).min(ACCENTS.len() - 1)];
    ui.ctx.update_config(|c| {
        c.audio.replaygain = rg;
        c.audio.preamp_db = preamp;
        c.audio.restore_session = app.get_restore_session();
        c.library.watch = app.get_watch_library();
        let name = app.get_renderer_name().trim().to_string();
        if !name.is_empty() {
            c.network.name = name;
        }
        c.network.renderer = app.get_renderer_enabled();
        c.network.media_server = app.get_server_enabled();
        c.online.lyrics = app.get_lyrics_online();
        c.online.cover_art = app.get_covers_online();
        c.ui.notifications = app.get_notifications();
        c.ui.close_to_tray = app.get_close_to_tray();
        c.ui.theme = if app.get_theme_dark() {
            ThemeCfg::Dark
        } else {
            ThemeCfg::Light
        };
        c.ui.adaptive_colors = app.get_adaptive_colors();
        c.ui.accent = accent.into();
    });
    ui.ctx.ctl.set_replaygain(rg, preamp);
    ui.loader.borrow().set_online(app.get_covers_online());
    apply_theme(ui);
}

// ------------------------------------------------------------------ scrobbling

fn check_listenbrainz(ui: &Rc<Ui>, token: String) {
    ui.app().set_lb_status(t("Checking…").into());
    std::thread::spawn(move || {
        let r = ricercar_online::listenbrainz::ListenBrainz::new(token).validate_token();
        post(move |ui| {
            let s = match r {
                Ok(Some(user)) => format!("{} {user}", t("Connected as")),
                Ok(None) => t("Invalid token").into(),
                Err(e) => format!("{}: {e}", t("Network error")),
            };
            ui.app().set_lb_status(s.into());
        });
    });
}

fn lastfm_connect(ui: &Rc<Ui>) {
    let app = ui.app();
    let key = app.get_lastfm_key().trim().to_string();
    let secret = app.get_lastfm_secret().trim().to_string();
    if key.is_empty() || secret.is_empty() {
        ui.toast(
            t("Enter the API key and shared secret of your Last.fm API account first."),
            true,
        );
        return;
    }
    ui.ctx.update_config(|c| {
        c.scrobble.lastfm_api_key = key.clone();
        c.scrobble.lastfm_secret = secret.clone();
    });
    std::thread::spawn(move || {
        let lf = ricercar_online::lastfm::LastFm::new(key, secret);
        let r = lf.get_token();
        post(move |ui| match r {
            Ok(token) => {
                let url = lf.auth_url(&token);
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                ui.extras.borrow_mut().lastfm_token = Some(token);
                ui.app().set_lastfm_pending(true);
                ui.toast(
                    t("Approve ricercar in your browser, then click “I approved it”."),
                    false,
                );
            }
            Err(e) => ui.toast(format!("Last.fm: {e}"), true),
        });
    });
}

fn lastfm_finish(ui: &Rc<Ui>) {
    let Some(token) = ui.extras.borrow_mut().lastfm_token.take() else {
        return;
    };
    let (key, secret) = {
        let c = ui.ctx.config.read().unwrap();
        (
            c.scrobble.lastfm_api_key.clone(),
            c.scrobble.lastfm_secret.clone(),
        )
    };
    std::thread::spawn(move || {
        let r = ricercar_online::lastfm::LastFm::new(key, secret).get_session(&token);
        post(move |ui| {
            ui.app().set_lastfm_pending(false);
            match r {
                Ok(s) => {
                    ui.ctx.update_config(|c| {
                        c.scrobble.lastfm_session = s.key.clone();
                        c.scrobble.lastfm_user = s.name.clone();
                    });
                    ui.app()
                        .set_lastfm_status(format!("{} {}", t("Connected as"), s.name).into());
                }
                Err(e) => ui.toast(format!("{}: {e}", t("Authorization failed")), true),
            }
        });
    });
}

// ------------------------------------------------------------------ wiring

pub fn wire(ui: &Rc<Ui>) {
    apply_theme(ui);
    {
        let c = ui.ctx.config.read().unwrap().audio.clone();
        ui.ctx.ctl.set_replaygain(c.replaygain, c.preamp_db);
    }
    let app = ui.app();
    app.on_settings_changed(|| with_ui(settings_changed));
    app.on_select_device(|name| {
        with_ui(|ui| {
            let name = name.to_string();
            ui.ctx.update_config(|c| c.audio.device = name.clone());
            refresh_devices(ui);
            ui.toast(format!("{}: {name}", t("Output device")), false);
        })
    });
    app.on_refresh_devices(|| with_ui(|ui| refresh_devices(ui)));
    app.on_add_root(|| {
        std::thread::spawn(|| {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                post(move |ui| {
                    ui.ctx.update_config(|c| {
                        if !c.library.roots.contains(&dir) {
                            c.library.roots.push(dir.clone());
                        }
                    });
                    refresh_library_rows(ui);
                    ui.toast(t("Folder added; indexing…"), false);
                });
            }
        });
    });
    app.on_remove_root(|i| {
        with_ui(|ui| {
            ui.ctx.update_config(|c| {
                let i = i.max(0) as usize;
                if i < c.library.roots.len() {
                    c.library.roots.remove(i);
                }
            });
            refresh_library_rows(ui);
            ui.toast(t("Folder removed"), false);
        })
    });
    app.on_rescan(|| {
        with_ui(|ui| {
            ui.loader.borrow_mut().forget_missing();
            ui.ctx.rescan();
        })
    });
    app.on_lb_save(|| {
        with_ui(|ui| {
            let token = ui.app().get_lb_token().trim().to_string();
            ui.ctx
                .update_config(|c| c.scrobble.listenbrainz_token = token.clone());
            if token.is_empty() {
                ui.app().set_lb_status(t("Not connected").into());
            } else {
                check_listenbrainz(ui, token);
            }
        })
    });
    app.on_lastfm_connect(|| with_ui(lastfm_connect));
    app.on_lastfm_finish(|| with_ui(lastfm_finish));
    app.on_lastfm_disconnect(|| {
        with_ui(|ui| {
            ui.ctx.update_config(|c| {
                c.scrobble.lastfm_session.clear();
                c.scrobble.lastfm_user.clear();
            });
            ui.app().set_lastfm_status("".into());
        })
    });

    app.on_radio_search(|q| {
        with_ui(|ui| {
            ui.extras.borrow_mut().debounce.remove("radio");
            radio_text_search(ui, &q);
        })
    });
    app.on_radio_edited(|q| {
        with_ui(|ui| {
            let q = q.to_string();
            // Radio Browser is a shared community service: wait for a pause
            // and at least two letters before querying.
            if q.trim().chars().count() == 1 {
                return;
            }
            debounce(ui, "radio", 400, move |ui| radio_text_search(ui, &q));
        })
    });
    app.on_text_setting_edited(|| {
        with_ui(|ui| debounce(ui, "settings", 700, settings_changed))
    });
    app.on_lb_edited(|| {
        with_ui(|ui| {
            debounce(ui, "lb", 900, |ui| {
                let token = ui.app().get_lb_token().trim().to_string();
                // ListenBrainz tokens are UUIDs: validate once one is complete.
                if token.is_empty() || token.len() >= 32 {
                    ui.app().invoke_lb_save();
                }
            })
        })
    });
    app.on_radio_tag(|tag| {
        with_ui(|ui| {
            let query = StationQuery {
                tag: Some(tag.to_string()),
                limit: 80,
                ..Default::default()
            };
            radio_query(ui, query, format!("{} · {tag}", t("Stations")));
        })
    });
    app.on_radio_play(|list, i| {
        with_ui(|ui| {
            let s = {
                let ex = ui.extras.borrow();
                let v = if list == "favs" {
                    &ex.favs
                } else {
                    &ex.results
                };
                v.get(i.max(0) as usize).cloned()
            };
            if let Some(s) = s {
                play_station(ui, &s);
            }
        })
    });
    app.on_radio_fav(|list, i| {
        with_ui(|ui| {
            {
                let mut ex = ui.extras.borrow_mut();
                let s = if list == "favs" {
                    ex.favs.get(i.max(0) as usize).cloned()
                } else {
                    ex.results.get(i.max(0) as usize).cloned()
                };
                if let Some(s) = s {
                    if let Some(pos) = ex.favs.iter().position(|f| f.uuid == s.uuid) {
                        ex.favs.remove(pos);
                    } else {
                        ex.favs.push(s);
                    }
                }
            }
            save_favs(ui);
            show_stations(ui);
        })
    });
}
