//! Player bar, now-playing view, queue, lyrics and the periodic sync with
//! the controller.

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ricercar_audio::{DeviceKind, TransportStatus};
use ricercar_core::{CtlEvent, CtlState, Origin, PlayContext, Repeat, TrackInfo};
use ricercar_online::lyrics::{LyricLine as Line, LyricsSource};
use slint::{Model, ModelRc, VecModel};

use crate::app::{LARGE, THUMB, Ui, post, with_ui};
use crate::images::{Lookup, Source};
use crate::text::{khz, long_duration, mmss, quality, t};
use crate::{ChainHop, LyricLine, Page, QueueRow};

#[derive(Default)]
pub struct PlayerView {
    /// Queue item id of the track shown in the bar.
    now_id: Option<u64>,
    now_title: Option<String>,
    lyrics: Vec<Line>,
    synced: bool,
    lyric_index: i32,
    chain_sig: String,
    lib_rev_seen: u64,
    lib_rev_changed_at: Option<Instant>,
    last_reload: Option<Instant>,
    was_scanning: bool,
    notif_id: u32,
    events: Option<std::sync::mpsc::Receiver<CtlEvent>>,
}

fn cover_source(info: &TrackInfo) -> Option<(String, Source)> {
    let hint = info.cover.clone()?;
    if hint.starts_with("http://") || hint.starts_with("https://") {
        return Some((hint.clone(), Source::Url(hint)));
    }
    let key = info.album_id.clone().unwrap_or_else(|| format!("p:{hint}"));
    Some((
        key.clone(),
        Source::Track {
            key,
            path: hint.into(),
        },
    ))
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_play_pause(|| with_ui(|ui| ui.ctx.ctl.toggle()));
    app.on_next(|| with_ui(|ui| ui.ctx.ctl.next()));
    app.on_prev(|| with_ui(|ui| ui.ctx.ctl.prev()));
    app.on_seek(|ms| {
        with_ui(|ui| {
            ui.ctx.ctl.seek_ms(ms.max(0) as u64);
            ui.app().set_pos_ms(ms.max(0));
        })
    });
    app.on_set_volume(|v| {
        with_ui(|ui| {
            let pct = (v.clamp(0.0, 1.0) * 100.0).round() as u32;
            ui.ctx.ctl.set_volume(pct);
            if ui.ctx.ctl.lock().muted && pct > 0 {
                ui.ctx.ctl.set_muted(false);
            }
            ui.app().set_volume(pct as f32 / 100.0);
        })
    });
    app.on_toggle_mute(|| {
        with_ui(|ui| {
            let m = ui.ctx.ctl.lock().muted;
            ui.ctx.ctl.set_muted(!m);
        })
    });
    app.on_toggle_shuffle(|| {
        with_ui(|ui| {
            let s = ui.ctx.ctl.lock().shuffle;
            ui.ctx.ctl.set_shuffle(!s);
        })
    });
    app.on_cycle_repeat(|| {
        with_ui(|ui| {
            let r = ui.ctx.ctl.lock().repeat;
            ui.ctx.ctl.set_repeat(r.cycle());
        })
    });
    app.on_toggle_fav(|| {
        with_ui(|ui| {
            let path = ui.st.borrow().now_path.clone();
            if let Some(p) = path {
                let fav = !ui.ctx.lib.is_favorite(&p);
                ui.ctx.lib.set_favorite(&p, fav);
                ui.app().set_fav(fav);
                ui.toast(
                    if fav {
                        t("Added to favorites")
                    } else {
                        t("Removed from favorites")
                    },
                    false,
                );
            }
        })
    });
    app.on_go_to_album(|| {
        with_ui(|ui| {
            let id = ui.app().get_album_id();
            if !id.is_empty() {
                ui.navigate(Page::Album, &id, true);
            }
        })
    });
    app.on_go_to_artist(|| {
        with_ui(|ui| {
            let info = ui.ctx.ctl.lock().track();
            if let Some(i) = info
                && i.path.is_some()
            {
                let name = i.album_artist.or(i.artist).unwrap_or_default();
                ui.navigate(Page::Artist, &name, true);
            }
        })
    });
    app.on_queue_play(|id| with_ui(|ui| ui.ctx.ctl.play_id(id as u64)));
    app.on_queue_remove(|id| with_ui(|ui| ui.ctx.ctl.remove_ids(&[id as u64])));
    app.on_queue_move(|from, to| {
        with_ui(|ui| {
            ui.ctx
                .ctl
                .move_item(from.max(0) as usize, to.max(0) as usize)
        })
    });
    app.on_queue_clear(|| with_ui(|ui| ui.ctx.ctl.clear_upcoming()));

    ui.player.borrow_mut().events = Some(ui.ctx.ctl.subscribe());
    ui.player.borrow_mut().lib_rev_seen = ui.ctx.lib.revision();

    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(120),
        || with_ui(tick),
    );
    ui.timers.borrow_mut().push(timer);
    tick(ui);
    rebuild_queue(ui, &ui.ctx.ctl.lock().clone());
}

fn tick(ui: &Rc<Ui>) {
    let st = ui.ctx.ctl.lock().clone();
    let app = ui.app();

    // ---- controller events
    let events: Vec<CtlEvent> = {
        let pv = ui.player.borrow();
        pv.events
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    };
    let mut queue_changed = false;
    for ev in events {
        match ev {
            CtlEvent::QueueChanged => queue_changed = true,
            CtlEvent::Error(msg) => ui.toast(short_error(&msg), true),
            CtlEvent::Played(_) if app.get_page() == Page::Home => {
                ui.st.borrow_mut().lib_rev = 0;
            }
            _ => {}
        }
    }

    // ---- transport
    let playing = st.status == TransportStatus::Playing;
    let tray_dirty = app.get_playing() != playing;
    app.set_playing(playing);
    app.set_pos_ms(st.pos_ms.min(i32::MAX as u64) as i32);
    app.set_dur_ms(st.dur_ms.min(i32::MAX as u64) as i32);
    app.set_volume(st.volume as f32 / 100.0);
    app.set_muted(st.muted);
    app.set_shuffle(st.shuffle);
    app.set_repeat(match st.repeat {
        Repeat::Off => 0,
        Repeat::All => 1,
        Repeat::One => 2,
    });
    app.set_can_next(st.has_next());

    let item = st.current_item().cloned();
    let now_id = item.as_ref().map(|q| q.id);
    let title_now = st.stream_title.clone();
    let changed = {
        let pv = ui.player.borrow();
        pv.now_id != now_id || pv.now_title != title_now
    };
    if changed {
        on_track_changed(ui, &st, item.as_ref().map(|q| &q.info));
        let mut pv = ui.player.borrow_mut();
        pv.now_id = now_id;
        pv.now_title = title_now;
    }
    update_chain(ui, &st);
    if tray_dirty || changed {
        crate::app::sync_tray(ui);
    }

    if queue_changed || st.queue_rev != ui.st.borrow().queue_rev || changed {
        rebuild_queue(ui, &st);
    }

    // ---- lyrics line
    let idx = {
        let pv = ui.player.borrow();
        if pv.synced {
            ricercar_online::lyrics::line_at(&pv.lyrics, st.pos_ms + 250)
                .map(|i| i as i32)
                .unwrap_or(-1)
        } else {
            -1
        }
    };
    tracing::trace!(target: "lyrics", idx, pos = st.pos_ms, "lyric line");
    if idx != ui.player.borrow().lyric_index {
        ui.player.borrow_mut().lyric_index = idx;
        app.set_lyric_index(idx);
    }

    // ---- library changes & scan progress
    let lib = &ui.ctx.lib;
    let scanning = lib.progress.running.load(Ordering::Relaxed);
    app.set_scanning(scanning);
    app.set_scan_done(lib.progress.done.load(Ordering::Relaxed) as i32);
    app.set_scan_total(lib.progress.total.load(Ordering::Relaxed) as i32);
    let rev = lib.revision();
    let reload = {
        let mut pv = ui.player.borrow_mut();
        let forced = ui.st.borrow().lib_rev == 0;
        if rev != pv.lib_rev_seen || forced {
            pv.lib_rev_seen = rev;
            pv.lib_rev_changed_at.get_or_insert(Instant::now());
            ui.st.borrow_mut().lib_rev = rev;
        }
        let settled = pv.lib_rev_changed_at.is_some_and(|t| {
            t.elapsed() > Duration::from_millis(if scanning { 3000 } else { 600 })
        });
        let scan_finished = pv.was_scanning && !scanning;
        pv.was_scanning = scanning;
        if settled || scan_finished {
            pv.lib_rev_changed_at = None;
            pv.last_reload = Some(Instant::now());
            true
        } else {
            false
        }
    };
    if reload {
        crate::views::refresh_stats(ui);
        crate::views::refresh_playlists(ui);
        // Pages that depend on the library; not while the user types a search.
        if app.get_page() != Page::Search && app.get_page() != Page::Settings {
            ui.reload_page();
        }
        if scanning || app.get_page() == Page::Settings {
            crate::extras::refresh_library_rows(ui);
        }
    }
}

fn short_error(msg: &str) -> String {
    let m = msg.rsplit(": ").next().unwrap_or(msg);
    let mut s = m.to_string();
    if s.len() > 140 {
        s.truncate(140);
        s.push('…');
    }
    s
}

fn on_track_changed(ui: &Rc<Ui>, st: &CtlState, info: Option<&TrackInfo>) {
    let app = ui.app();
    let Some(info) = info else {
        app.set_has_track(false);
        app.set_title("".into());
        app.set_artist("".into());
        app.set_album("".into());
        app.set_album_id("".into());
        app.set_cover(Default::default());
        app.set_cover_large(Default::default());
        app.set_backdrop(Default::default());
        ui.st.borrow_mut().cover_rgb = None;
        crate::extras::apply_cover_accent(ui);
        set_lyrics(ui, Vec::new(), false, t("Nothing is playing"), "");
        ui.st.borrow_mut().now_path = None;
        mark_playing(ui, None);
        return;
    };
    app.set_has_track(true);
    let live_title = st.stream_title.clone().filter(|_| info.live);
    match &live_title {
        Some(song) => {
            app.set_title(song.clone().into());
            app.set_artist(info.title.clone().into());
        }
        None => {
            app.set_title(info.title.clone().into());
            app.set_artist(
                info.artist
                    .clone()
                    .unwrap_or_else(|| crate::app::unknown_artist().into())
                    .into(),
            );
        }
    }
    app.set_album(info.album.clone().unwrap_or_default().into());
    app.set_album_id(
        info.path
            .as_ref()
            .and(info.album_id.clone())
            .unwrap_or_default()
            .into(),
    );
    app.set_live(info.live);
    app.set_origin(
        match (st.origin, info.live) {
            (_, true) => t("Internet radio"),
            (Origin::Remote, _) => t("UPnP"),
            _ => "",
        }
        .into(),
    );
    app.set_context_label(context_label(ui, &st.context).into());
    let fav = info
        .path
        .as_ref()
        .is_some_and(|p| ui.ctx.lib.is_favorite(p));
    app.set_fav(fav);

    let same_track = ui.st.borrow().now_path.as_deref() == info.path.as_deref()
        && info.path.is_some()
        && ui.player.borrow().now_id.is_some()
        && live_title.is_some();
    ui.st.borrow_mut().now_path = info.path.clone();
    mark_playing(ui, info.path.as_deref());

    // Covers: small one through the shared loader; large/backdrop/accent here.
    let src = cover_source(info);
    ui.st.borrow_mut().now_key = src.as_ref().map(|(k, _)| k.clone());
    match &src {
        Some((key, s)) => {
            let lookup = info
                .album
                .clone()
                .zip(info.artist.clone())
                .map(|(al, ar)| Lookup {
                    artist: info.album_artist.clone().unwrap_or(ar),
                    album: al,
                });
            app.set_cover(ui.cover(key, s.clone(), lookup, THUMB));
            if !same_track {
                app.set_cover_large(ui.loader.borrow().peek(key, LARGE).unwrap_or_default());
                let covers = ui.ctx.covers.clone();
                let s = s.clone();
                let key = key.clone();
                std::thread::spawn(move || {
                    let art = crate::images::now_art(&covers, &s);
                    post(move |ui| {
                        if ui.st.borrow().now_key.as_deref() != Some(key.as_str()) {
                            return;
                        }
                        let app = ui.app();
                        if let Some(b) = art.large {
                            app.set_cover_large(slint::Image::from_rgba8(b));
                        }
                        app.set_backdrop(
                            art.backdrop
                                .map(slint::Image::from_rgba8)
                                .unwrap_or_default(),
                        );
                        ui.st.borrow_mut().cover_rgb = art.accent;
                        crate::extras::apply_cover_accent(ui);
                    });
                });
            }
        }
        None => {
            app.set_cover(Default::default());
            app.set_cover_large(Default::default());
            app.set_backdrop(Default::default());
            ui.st.borrow_mut().cover_rgb = None;
            crate::extras::apply_cover_accent(ui);
        }
    }

    if !same_track {
        fetch_lyrics(ui, info, live_title.as_deref());
    }
    notify(ui, info, live_title.as_deref());
}

fn context_label(ui: &Ui, ctx: &PlayContext) -> String {
    let lib = &ui.ctx.lib;
    let what = match ctx {
        PlayContext::Album(id) => lib.album(id).map(|a| a.title),
        PlayContext::Artist(name) => Some(name.clone()),
        PlayContext::Playlist(id) => lib.playlist(*id).map(|p| p.name),
        PlayContext::Radio => Some(t("Internet radio").into()),
        PlayContext::Mix => Some(t("Library mix").into()),
        PlayContext::None => None,
    };
    what.map(|w| format!("{} {w}", t("Playing from")))
        .unwrap_or_default()
}

fn mark_playing(ui: &Ui, path: Option<&str>) {
    for m in ui.models.track_models() {
        for i in 0..m.row_count() {
            let mut r = m.row_data(i).unwrap();
            let p = path.is_some_and(|p| r.key == p);
            if r.playing != p {
                r.playing = p;
                m.set_row_data(i, r);
            }
        }
    }
}

pub fn sync_fav(ui: &Ui) {
    let path = ui.st.borrow().now_path.clone();
    if let Some(p) = path {
        ui.app().set_fav(ui.ctx.lib.is_favorite(&p));
    }
}

pub fn refill_now_cover(ui: &Ui) {
    let key = ui.st.borrow().now_key.clone();
    let app = ui.app();
    if let Some(k) = key
        && app.get_cover().size().width == 0
        && let Some(i) = ui.loader.borrow().peek(&k, THUMB)
    {
        app.set_cover(i);
    }
}

fn update_chain(ui: &Ui, st: &CtlState) {
    let app = ui.app();
    let info = st.track();
    let chain = ui.ctx.ctl.engine_chain();
    let c = &chain;
    let fmt = c.format;
    let sig = format!(
        "{:?}{:?}{}{}{}{}{:?}",
        fmt,
        c.container,
        c.bit_perfect,
        st.volume,
        st.muted,
        c.device,
        info.as_ref().map(|i| &i.uri)
    );
    if ui.player.borrow().chain_sig == sig {
        return;
    }
    ui.player.borrow_mut().chain_sig = sig;
    let Some(info) = info else {
        app.set_quality("".into());
        app.set_codec("".into());
        app.set_chain(ModelRc::default());
        app.set_chain_short(ModelRc::default());
        app.set_chain_known(false);
        return;
    };
    // Prefer what the decoder actually produced over tag hints.
    let rate = fmt.map(|f| f.sample_rate).or(info.sample_rate);
    let bits = fmt.map(|f| f.bits).or(info.bits);
    let lossy = matches!(
        info.codec.as_deref(),
        Some("MP3" | "AAC" | "Vorbis" | "Opus")
    );
    let q = if lossy {
        quality(rate, None, None, None)
    } else {
        quality(rate, bits, info.codec.as_deref(), None)
    };
    app.set_quality(q.into());
    app.set_codec(info.codec.clone().unwrap_or_default().into());
    app.set_hires(!lossy && ricercar_core::library::is_hires(rate, bits));
    app.set_chain_known(fmt.is_some());
    // Bit-perfect means untouched samples reaching a real DAC: a null or
    // shared device never qualifies, whatever the engine's own flag says.
    let hardware = c.device_kind == DeviceKind::Hardware;
    app.set_bitperfect(fmt.is_some() && c.bit_perfect && hardware);
    app.set_device_label(c.device.clone().into());

    let mut hops = Vec::new();
    let src_val = match (info.codec.as_deref(), fmt) {
        (codec, Some(f)) => format!(
            "{}{} kHz · {} ch{}",
            codec.map(|c| format!("{c} · ")).unwrap_or_default(),
            khz(f.sample_rate),
            f.channels,
            if lossy { " · lossy" } else { "" }
        ),
        (Some(c), None) => c.to_string(),
        _ => "—".into(),
    };
    hops.push(ChainHop {
        label: t("Source").into(),
        value: src_val.into(),
        state: if lossy { 1 } else { 0 },
    });
    if let Some(f) = fmt {
        hops.push(ChainHop {
            label: t("Decoder").into(),
            value: format!("{} {}-bit integer PCM", t("Decoded to"), f.bits).into(),
            state: if lossy { 1 } else { 0 },
        });
    }
    // Volume only appears when it changes the samples.
    let gain_touch = st.muted || st.volume < 100 || (!c.bit_perfect && hardware && c.volume == 100);
    if gain_touch {
        let value = if st.muted {
            t("Muted").to_string()
        } else if st.volume < 100 {
            format!("{} {} %", t("Software volume"), st.volume)
        } else {
            t("ReplayGain / preamp").to_string()
        };
        hops.push(ChainHop {
            label: t("Processing").into(),
            value: value.into(),
            state: 1,
        });
    }
    if let Some(f) = fmt {
        hops.push(ChainHop {
            label: t("Output format").into(),
            value: format!(
                "{} · {} kHz · {} ch",
                c.container.unwrap_or("?"),
                khz(f.sample_rate),
                f.channels
            )
            .into(),
            state: 0,
        });
    }
    let (dev_desc, dev_state) = match c.device_kind {
        DeviceKind::Hardware => (format!("{} · {}", c.device, t("exclusive, no mixer")), 0),
        DeviceKind::Virtual => (
            format!("{} · {}", c.device, t("shared: may resample or mix")),
            1,
        ),
        DeviceKind::Null => (t("Null sink: audio discarded").to_string(), 1),
        DeviceKind::File => (format!("{} · {}", c.device, t("written to a file")), 1),
    };
    hops.push(ChainHop {
        label: t("Device").into(),
        value: dev_desc.into(),
        state: dev_state,
    });
    app.set_chain(ModelRc::new(VecModel::from(hops)));

    // Compact form for the player bar: FLAC 24/96 → Vol 73 % → S24_LE → hw:1,0
    let mut short = vec![ChainHop {
        label: "".into(),
        value: match info.codec.as_deref() {
            Some(c) => format!("{c} {}", app.get_quality()),
            None => app.get_quality().to_string(),
        }
        .trim()
        .to_string()
        .into(),
        state: if lossy { 1 } else { 0 },
    }];
    if gain_touch {
        short.push(ChainHop {
            label: "".into(),
            value: if st.muted {
                t("Muted").to_string()
            } else if st.volume < 100 {
                format!("{} %", st.volume)
            } else {
                "ReplayGain".to_string()
            }
            .into(),
            state: 1,
        });
    }
    if fmt.is_some() {
        short.push(ChainHop {
            label: "".into(),
            value: c.container.unwrap_or("?").into(),
            state: 0,
        });
    }
    short.push(ChainHop {
        label: "".into(),
        value: match c.device_kind {
            DeviceKind::Null => t("Null sink").into(),
            _ => c.device.clone().into(),
        },
        state: dev_state,
    });
    app.set_chain_short(ModelRc::new(VecModel::from(short)));
}

fn rebuild_queue(ui: &Rc<Ui>, st: &CtlState) {
    ui.st.borrow_mut().queue_rev = st.queue_rev;
    let cur = st.current;
    let rows: Vec<QueueRow> = st
        .queue
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let (cover, ckey) = match cover_source(&q.info) {
                Some((k, s)) => (ui.cover(&k, s, None, THUMB), k),
                None => (Default::default(), String::new()),
            };
            QueueRow {
                id: q.id as i32,
                title: q.info.title.clone().into(),
                artist: q.info.artist.clone().unwrap_or_default().into(),
                dur: if q.info.duration_ms > 0 {
                    mmss(q.info.duration_ms)
                } else {
                    String::new()
                }
                .into(),
                cover,
                ckey: ckey.into(),
                current: Some(i) == cur,
                past: cur.is_some_and(|c| i < c),
            }
        })
        .collect();
    let upcoming: Vec<_> = st
        .queue
        .iter()
        .skip(cur.map(|c| c + 1).unwrap_or(0))
        .collect();
    let dur: u64 = upcoming.iter().map(|q| q.info.duration_ms).sum();
    let app = ui.app();
    app.set_queue_meta(
        format!(
            "{}{}",
            crate::text::count(upcoming.len(), "track", "tracks"),
            if dur > 0 {
                format!(" · {}", long_duration(dur))
            } else {
                String::new()
            }
        )
        .into(),
    );
    app.set_queue_current(cur.map(|c| c as i32).unwrap_or(-1));
    ui.queue_model.set_vec(rows);
}

// ------------------------------------------------------------------ lyrics

fn set_lyrics(ui: &Ui, lines: Vec<Line>, synced: bool, status: &str, source: &str) {
    let app = ui.app();
    let rows: Vec<LyricLine> = lines
        .iter()
        .map(|l| LyricLine {
            text: l.text.clone().into(),
            ms: l.time_ms.min(i32::MAX as u64) as i32,
        })
        .collect();
    app.set_lyrics(ModelRc::new(VecModel::from(rows)));
    app.set_lyrics_synced(synced);
    app.set_lyrics_status(status.into());
    app.set_lyrics_source(source.into());
    app.set_lyric_index(-1);
    let mut pv = ui.player.borrow_mut();
    pv.lyrics = lines;
    pv.synced = synced;
    pv.lyric_index = -1;
}

fn plain_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|l| Line {
            time_ms: 0,
            text: l.trim().to_string(),
        })
        .collect()
}

fn fetch_lyrics(ui: &Rc<Ui>, info: &TrackInfo, live_title: Option<&str>) {
    set_lyrics(ui, Vec::new(), false, t("Searching for lyrics…"), "");
    let online = ui.ctx.config.read().unwrap().online.lyrics;
    let path = info.path.clone();
    let (artist, title) = match live_title {
        // "Artist - Title" is the usual ICY convention.
        Some(s) => match s.split_once(" - ") {
            Some((a, b)) => (Some(a.trim().to_string()), b.trim().to_string()),
            None => (None, s.to_string()),
        },
        None => (info.artist.clone(), info.title.clone()),
    };
    let album = if live_title.is_some() {
        None
    } else {
        info.album.clone()
    };
    let dur = (info.duration_ms > 0).then_some((info.duration_ms / 1000) as u32);
    let uri = info.uri.clone();
    let cache_dir = ricercar_core::config::cache_dir().join("lyrics");
    std::thread::spawn(move || {
        let mut found: Option<(Vec<Line>, bool, &'static str)> = None;
        if let Some(p) = &path {
            let p = std::path::Path::new(p);
            if let Some(text) = ricercar_core::meta::local_lyrics(p) {
                let synced = ricercar_online::lyrics::parse_lrc(&text);
                let from_lrc = p.with_extension("lrc").exists();
                let src = if from_lrc {
                    "Lyrics from the .lrc file"
                } else {
                    "Lyrics from the file"
                };
                found = Some(if synced.is_empty() {
                    (plain_lines(&text), false, src)
                } else {
                    (synced, true, src)
                });
            }
        }
        let mut status = "No lyrics for this track";
        if found.is_none() {
            match (&artist, online) {
                (Some(a), true) => {
                    let cache = ricercar_online::lyrics::LyricsCache::new(cache_dir);
                    match ricercar_online::lyrics::fetch_cached(
                        &cache,
                        a,
                        &title,
                        album.as_deref(),
                        dur,
                    ) {
                        Ok(Some(l)) if l.instrumental => status = "Instrumental",
                        Ok(Some(l)) => {
                            let src = match l.source {
                                LyricsSource::Lrclib => "Lyrics from lrclib.net",
                                _ => "Lyrics from the file",
                            };
                            found = match (l.synced, l.plain) {
                                (Some(s), _) if !s.is_empty() => Some((s, true, src)),
                                (_, Some(p)) => Some((plain_lines(&p), false, src)),
                                _ => None,
                            };
                        }
                        Ok(None) => {}
                        Err(e) => tracing::debug!("lyrics: {e}"),
                    }
                }
                (_, false) => status = "Online lyrics are turned off in Settings",
                _ => {}
            }
        }
        post(move |ui| {
            // Ignore answers for a track that is no longer playing.
            if ui.ctx.ctl.lock().current_uri().as_deref() != Some(uri.as_str()) {
                return;
            }
            match found {
                Some((lines, synced, src)) => set_lyrics(ui, lines, synced, "", t(src)),
                None => set_lyrics(ui, Vec::new(), false, t(status), ""),
            }
        });
    });
}

// ------------------------------------------------------------------ notifications

fn notify(ui: &Rc<Ui>, info: &TrackInfo, live_title: Option<&str>) {
    if !ui.ctx.config.read().unwrap().ui.notifications {
        return;
    }
    let summary = live_title.unwrap_or(&info.title).to_string();
    let body = if live_title.is_some() {
        info.title.clone()
    } else {
        [info.artist.clone(), info.album.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" — ")
    };
    let icon = info
        .cover
        .as_ref()
        .filter(|c| !c.starts_with("http"))
        .and_then(|c| {
            let key = info.album_id.clone().unwrap_or_else(|| format!("p:{c}"));
            ui.ctx.covers.thumb(&key, std::path::Path::new(c), 128)
        })
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "ricercar".into());
    let replace = ui.player.borrow().notif_id;
    std::thread::spawn(move || {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return;
        };
        let hints: std::collections::HashMap<&str, zbus::zvariant::Value> = [
            ("desktop-entry", zbus::zvariant::Value::from("ricercar")),
            ("transient", zbus::zvariant::Value::from(true)),
            ("category", zbus::zvariant::Value::from("x-gnome.music")),
        ]
        .into_iter()
        .collect();
        let r = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "ricercar",
                replace,
                icon.as_str(),
                summary.as_str(),
                body.as_str(),
                Vec::<&str>::new(),
                hints,
                4000i32,
            ),
        );
        if let Ok(m) = r
            && let Ok((id,)) = m.body().deserialize::<(u32,)>()
        {
            post(move |ui| ui.player.borrow_mut().notif_id = id);
        }
    });
}
