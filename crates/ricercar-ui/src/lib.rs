use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ricercar_core::{Controller, Library, Track};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

#[derive(Clone)]
struct Row {
    item: TrackItem,
    track: Track,
}

fn fmt_time(ms: u64) -> String {
    format!("{:02}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

fn load_rows(lib: &Library, q: &str) -> Vec<Row> {
    let tracks = if q.is_empty() {
        let mut all = Vec::new();
        for a in lib.albums() {
            all.extend(lib.album_tracks(&a.album, a.album_artist.as_deref()));
        }
        all.sort_by(|a, b| {
            a.artist
                .cmp(&b.artist)
                .then(a.album.cmp(&b.album))
                .then(a.track.cmp(&b.track))
        });
        all
    } else {
        lib.search(q)
    };
    tracks
        .into_iter()
        .map(|t| Row {
            item: TrackItem {
                title: t.title.clone().into(),
                artist: t.artist.clone().unwrap_or_default().into(),
                album: t.album.clone().unwrap_or_default().into(),
                dur: fmt_time(t.duration_ms).into(),
            },
            track: t,
        })
        .collect()
}

fn set_model(ui: &App, rows: &RefCell<Vec<Row>>, new: Vec<Row>) {
    let items: Vec<TrackItem> = new.iter().map(|r| r.item.clone()).collect();
    ui.set_tracks(ModelRc::from(Rc::new(VecModel::<TrackItem>::from_iter(
        items,
    ))));
    *rows.borrow_mut() = new;
}

pub fn run_ui(ctl: Arc<Controller>) -> Result<(), slint::PlatformError> {
    let ui = App::new()?;
    let rows = Rc::new(RefCell::new(Vec::<Row>::new()));

    set_model(&ui, &rows, load_rows(&ctl.lib, ""));

    ui.on_play_pause({
        let c = ctl.clone();
        Box::new(move || c.toggle())
    });
    ui.on_stop({
        let c = ctl.clone();
        Box::new(move || c.stop())
    });
    ui.on_next({
        let c = ctl.clone();
        Box::new(move || c.next())
    });
    ui.on_prev({
        let c = ctl.clone();
        Box::new(move || c.prev())
    });
    ui.on_volume_changed({
        let c = ctl.clone();
        Box::new(move |v: f32| c.set_volume(((v as f64) * 100.0).clamp(0.0, 100.0) as u32))
    });
    ui.on_activate({
        let c = ctl.clone();
        let rows = rows.clone();
        Box::new(move |i: i32| {
            let tracks: Vec<Track> = rows.borrow().iter().map(|r| r.track.clone()).collect();
            let idx = i.max(0) as usize;
            if idx < tracks.len() {
                c.play_tracks(tracks, idx);
            }
        })
    });
    ui.on_search_changed({
        let c = ctl.clone();
        let ui = ui.as_weak();
        let rows = rows.clone();
        Box::new(move |q: SharedString| {
            let new = load_rows(&c.lib, q.as_str());
            if let Some(u) = ui.upgrade() {
                set_model(&u, &rows, new);
            }
        })
    });

    let weak = ui.as_weak();
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        std::time::Duration::from_millis(300),
        {
            let c = ctl.clone();
            move || {
                let Some(u) = weak.upgrade() else { return };
                let st = c.state.lock().unwrap();
                let t = st.track();
                u.set_track_title(
                    t.as_ref()
                        .map(|x| x.title.clone())
                        .unwrap_or_else(|| "—".into())
                        .into(),
                );
                u.set_track_artist(
                    t.as_ref()
                        .and_then(|x| x.artist.clone())
                        .unwrap_or_default()
                        .into(),
                );
                u.set_position_text(fmt_time(st.pos_ms).into());
                u.set_playing(matches!(
                    st.status,
                    ricercar_audio::TransportStatus::Playing
                ));
                u.set_volume(st.volume as f32 / 100.0);
                let origin = match st.origin {
                    ricercar_core::Origin::Remote => "remote (upnp)",
                    ricercar_core::Origin::Local => "local",
                };
                u.set_status_text(format!("{} · {}", origin, c.device_name()).into());
            }
        },
    );

    ui.run()
}
