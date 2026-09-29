//! Headless screenshots for development and docs: `RICERCAR_SNAPSHOT=<dir>`
//! renders the window with Slint's software renderer (no display needed),
//! walks through the main views and writes one PNG per view.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::Rgb8Pixel;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{EventLoopProxy, Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};

use crate::Page;
use crate::app::{Ui, with_ui};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Clone, Default)]
struct Queue {
    jobs: Arc<Mutex<VecDeque<Job>>>,
    quit: Arc<AtomicBool>,
}

struct Proxy(Queue);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        self.0.quit.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Job) -> Result<(), slint::EventLoopError> {
        self.0.jobs.lock().unwrap().push_back(event);
        Ok(())
    }
}

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
    queue: Queue,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.queue.clone())))
    }
    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        while !self.queue.quit.load(Ordering::SeqCst) {
            slint::platform::update_timers_and_animations();
            let jobs: Vec<Job> = self.queue.jobs.lock().unwrap().drain(..).collect();
            for j in jobs {
                j();
            }
            // Render continuously like a real display so animations advance.
            if self.window.has_active_animations() {
                self.window.request_redraw();
            }
            let size = self.window.size();
            let mut scratch = vec![Rgb8Pixel::default(); (size.width * size.height) as usize];
            if self.window.draw_if_needed(|r| {
                r.render(&mut scratch, size.width as usize);
            }) {
                crate::profile::frame_rendered();
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        Ok(())
    }
}

thread_local! {
    static WINDOW: std::cell::RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { std::cell::RefCell::new(None) };
}

pub const W: u32 = 1440;
pub const H: u32 = 900;

/// Install the headless platform; must run before the window is created.
pub fn install() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(PhysicalSize::new(W, H));
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    slint::platform::set_platform(Box::new(Headless {
        window,
        start: Instant::now(),
        queue: Queue::default(),
    }))
    .expect("platform already set");
}

fn capture(dir: &std::path::Path, name: &str) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(win) = w.as_ref() else { return };
        win.request_redraw();
        let mut buf = vec![Rgb8Pixel::default(); (W * H) as usize];
        win.draw_if_needed(|r| {
            r.render(&mut buf, W as usize);
        });
        let raw: Vec<u8> = buf.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        let path = dir.join(format!("{name}.png"));
        if let Some(img) = image::RgbImage::from_raw(W, H, raw) {
            let _ = img.save(&path);
            eprintln!("snapshot: {}", path.display());
        }
    });
}

type Step = (u64, Box<dyn Fn(&Rc<Ui>)>);

/// `RICERCAR_SNAPSHOT_TOUR`: the default screenshot tour, `perf` or `state`.
pub fn tour() -> String {
    std::env::var("RICERCAR_SNAPSHOT_TOUR").unwrap_or_default()
}

/// `RICERCAR_SNAPSHOT_TOUR=state`: print the interface state restored at
/// startup, then leave the app 1600×1000 on the Artists page with sorts and
/// the queue drawer changed. Run twice: the second run must print that state.
fn state_tour() {
    slint::Timer::single_shot(Duration::from_millis(500), || {
        with_ui(|ui| {
            let st = crate::app::current_state(ui);
            eprintln!(
                "state: restored {}",
                serde_json::to_string(&st).unwrap_or_default()
            );
            let app = ui.app();
            app.set_album_sort(1);
            app.set_track_sort(2);
            app.set_queue_open(true);
            ui.window
                .window()
                .set_size(slint::LogicalSize::new(1600.0, 1000.0));
            ui.navigate(Page::Artists, "", true);
        });
        slint::Timer::single_shot(Duration::from_millis(500), || {
            let _ = slint::quit_event_loop();
        });
    });
}

/// Scripted tour: each step runs after its delay, then the view is captured.
pub fn start(ui: &Rc<Ui>, dir: std::path::PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    match tour().as_str() {
        "perf" => return perf_tour(dir),
        "state" => return state_tour(),
        "plugins" => return plugins_tour(dir),
        _ => {}
    }
    let _ = ui;
    let album = |title: &'static str| {
        move || {
            with_ui_ret(|ui| {
                ui.ctx
                    .lib
                    .albums(ricercar_core::AlbumSort::Title)
                    .into_iter()
                    .find(|a| a.title == title)
                    .map(|a| a.id)
            })
            .flatten()
            .unwrap_or_default()
        }
    };
    let neon = album("Neon Rain");
    let fugues = album("Fugues & Ricercars");
    let steps: Vec<(&str, Step)> = vec![
        (
            "home",
            (
                4500,
                Box::new(move |ui| {
                    seed_demo(ui);
                    ui.navigate(Page::Home, "", true);
                }),
            ),
        ),
        (
            "album",
            (
                1500,
                Box::new(move |ui| {
                    let id = fugues();
                    let tracks = ui.ctx.lib.album_tracks(&id);
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_library_tracks(
                        &tracks,
                        0,
                        ricercar_core::PlayContext::Album(id.clone()),
                    );
                    ctl.pause();
                    ui.navigate(Page::Album, &id, true);
                    slint::Timer::single_shot(Duration::from_millis(300), move || {
                        ctl.seek_ms(9_000);
                    });
                }),
            ),
        ),
        (
            "home-playing",
            (1500, Box::new(|ui| ui.navigate(Page::Home, "", true))),
        ),
        (
            "albums",
            (1200, Box::new(|ui| ui.navigate(Page::Albums, "", true))),
        ),
        (
            "artist",
            (
                1200,
                Box::new(|ui| ui.navigate(Page::Artist, "Ensemble Lumen", true)),
            ),
        ),
        (
            "lyrics",
            (
                2500,
                Box::new(move |ui| {
                    // The null sink is unpaced: pause in the same tick as the
                    // load so the engine never races past the first track.
                    let tracks = ui.ctx.lib.album_tracks(&neon());
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_library_tracks(&tracks, 0, ricercar_core::PlayContext::None);
                    ctl.pause();
                    slint::Timer::single_shot(Duration::from_millis(300), move || {
                        ctl.seek_ms(10_800);
                    });
                    let app = ui.app();
                    app.set_np_tab(0);
                    app.set_now_playing_open(true);
                }),
            ),
        ),
        ("signal-path", (900, Box::new(|ui| ui.app().set_np_tab(2)))),
        (
            "queue",
            (
                900,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_now_playing_open(false);
                    app.set_queue_open(true);
                    ui.navigate(Page::Tracks, "", true);
                }),
            ),
        ),
        (
            "search",
            (
                1500,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_queue_open(false);
                    app.set_search_text("blue".into());
                    app.invoke_search("blue".into());
                }),
            ),
        ),
        (
            "genres",
            (1200, Box::new(|ui| ui.navigate(Page::Genres, "", true))),
        ),
        (
            "playlist",
            (
                1200,
                Box::new(|ui| {
                    let id = ui
                        .ctx
                        .lib
                        .playlists()
                        .iter()
                        .find(|p| p.name == "Late night")
                        .map(|p| p.id)
                        .unwrap_or(0);
                    ui.navigate(Page::Playlist, &id.to_string(), true);
                }),
            ),
        ),
        (
            "settings",
            (
                1200,
                Box::new(|ui| {
                    // Tours never open real hardware: show example
                    // capabilities on the first hw: device.
                    let hw = ricercar_audio::device::list_devices()
                        .into_iter()
                        .find(|d| d.kind == ricercar_audio::DeviceKind::Hardware);
                    if let Some(d) = hw {
                        let caps = crate::dac::CachedCaps {
                            rates: vec![44_100, 48_000, 88_200, 96_000],
                            formats: vec!["S16_LE".into(), "S24_3LE".into(), "S32_LE".into()],
                            channels_min: 2,
                            channels_max: 2,
                            probed_at: 0,
                        };
                        crate::extras::store_caps(ui, &d.name, caps);
                        ui.extras.borrow_mut().expanded.insert(d.name);
                    }
                    ui.navigate(Page::Settings, "", true);
                }),
            ),
        ),
        (
            "settings-about",
            (
                900,
                Box::new(|ui| {
                    ui.app().set_settings_at_end(true);
                }),
            ),
        ),
        (
            "album-light",
            (
                1200,
                Box::new(move |ui| {
                    ui.app().set_theme_dark(false);
                    ui.window.global::<crate::Theme>().set_dark(false);
                    let id = neon();
                    ui.navigate(Page::Album, &id, true);
                }),
            ),
        ),
    ];
    let mut delay = 0u64;
    let dir = Rc::new(dir);
    let n = steps.len();
    for (i, (name, (wait, f))) in steps.into_iter().enumerate() {
        delay += wait;
        let dir = dir.clone();
        let f = Rc::new(f);
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            with_ui(|ui| f(ui));
        });
        delay += 1600;
        let name = name.to_string();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            with_ui(|ui| ui.refill_covers());
            capture(&dir, &name);
            if i + 1 == n {
                let _ = slint::quit_event_loop();
            }
        });
    }
}

/// Plays, favorites and a playlist so the demo library looks lived-in.
fn seed_demo(ui: &Rc<Ui>) {
    let lib = &ui.ctx.lib;
    if !lib.playlists().is_empty() {
        return;
    }
    let tracks = lib.tracks(ricercar_core::TrackSort::Title);
    for (i, t) in tracks.iter().enumerate() {
        for _ in 0..(i * 7 % 5) {
            lib.record_play(&t.path);
        }
        if i % 6 == 0 {
            lib.set_favorite(&t.path, true);
        }
    }
    let id = lib.create_playlist("Late night");
    let paths: Vec<String> = tracks.iter().step_by(4).map(|t| t.path.clone()).collect();
    lib.add_to_playlist(id, &paths);
    lib.create_playlist("Focus");
    if let Some(a) = lib.albums(ricercar_core::AlbumSort::Title).first() {
        lib.set_album_favorite(&a.id, true);
    }
    crate::views::refresh_playlists(ui);
}

fn with_ui_ret<T>(f: impl FnOnce(&Rc<Ui>) -> T) -> Option<T> {
    let mut out = None;
    with_ui(|ui| out = Some(f(ui)));
    out
}

// ------------------------------------------------------------------ perf tour

/// `RICERCAR_SNAPSHOT_TOUR=perf` (with `RICERCAR_PROFILE=1`): waits for the
/// startup scan, then walks the heavy views of a big library and prints
/// timings and memory (see `scripts/perf.sh` and `docs/perf.md`).
fn perf_tour(dir: std::path::PathBuf) {
    fn wait_for_scan(dir: std::path::PathBuf) {
        slint::Timer::single_shot(Duration::from_millis(100), move || {
            let done = with_ui_ret(|ui| {
                let p = &ui.ctx.lib.progress;
                p.finished.load(Ordering::SeqCst) >= 1 && !p.running.load(Ordering::SeqCst)
            })
            .unwrap_or(true);
            if done {
                perf_steps(dir);
            } else {
                wait_for_scan(dir);
            }
        });
    }
    wait_for_scan(dir);
}

fn perf_steps(dir: std::path::PathBuf) {
    let steps: Vec<Step> = vec![
        (
            1500,
            Box::new(|ui| {
                crate::profile::note("library tracks", ui.ctx.lib.count());
                crate::profile::memory("memory idle");
                ui.navigate(Page::Albums, "", true);
            }),
        ),
        (
            1500,
            Box::new(|ui| {
                ui.app().set_album_sort(1);
                crate::profile::expect_frame("albums sort by artist");
                let _p = crate::profile::span("albums sort by artist: load");
                crate::views::load_albums(ui);
            }),
        ),
        (1500, Box::new(|ui| ui.navigate(Page::Artists, "", true))),
        (1500, Box::new(|ui| ui.navigate(Page::Tracks, "", true))),
        (
            3000,
            Box::new(|ui| {
                ui.app().set_track_sort(1);
                crate::profile::expect_frame("tracks sort by title");
                let _p = crate::profile::span("tracks sort by title: load");
                ui.app().invoke_tracks_sort_changed();
            }),
        ),
        (
            3000,
            Box::new(|ui| {
                let artist = ui
                    .ctx
                    .lib
                    .albums(ricercar_core::AlbumSort::Artist)
                    .into_iter()
                    .find(|a| a.artist != "Various Artists")
                    .map(|a| a.artist)
                    .unwrap_or_default();
                ui.navigate(Page::Artist, &artist, true);
            }),
        ),
        (
            1500,
            Box::new(|ui| {
                let id = ui
                    .ctx
                    .lib
                    .albums(ricercar_core::AlbumSort::Title)
                    .get(100)
                    .map(|a| a.id.clone())
                    .unwrap_or_default();
                ui.navigate(Page::Album, &id, true);
            }),
        ),
        (1500, Box::new(|ui| ui.navigate(Page::Genres, "", true))),
        (1500, Box::new(|ui| ui.navigate(Page::Search, "", true))),
        (1500, Box::new(perf_search)),
        (
            1500,
            Box::new(|ui| {
                // A long queue, paused before the unpaced null sink moves on.
                let tracks: Vec<_> = ui
                    .ctx
                    .lib
                    .tracks(ricercar_core::TrackSort::Album)
                    .into_iter()
                    .take(5000)
                    .collect();
                let infos = tracks.iter().map(ricercar_core::TrackInfo::from).collect();
                let ctl = ui.ctx.ctl.clone();
                ctl.play_tracks(infos, 0, ricercar_core::PlayContext::None);
                ctl.pause();
                ui.app().set_queue_open(true);
                crate::profile::expect_frame("queue of 5000: open");
            }),
        ),
        (
            1500,
            Box::new(|ui| {
                ui.ctx.ctl.play_index(1);
                ui.ctx.ctl.pause();
                crate::profile::expect_frame("next track with a 5000 queue");
            }),
        ),
        (
            1500,
            Box::new(|ui| {
                ui.app().set_queue_open(false);
                ui.navigate(Page::Home, "", true);
            }),
        ),
    ];
    let mut delay = 0;
    for (wait, f) in steps {
        delay += wait;
        slint::Timer::single_shot(Duration::from_millis(delay), move || with_ui(|ui| f(ui)));
    }
    slint::Timer::single_shot(Duration::from_millis(delay + 2000), move || {
        crate::profile::memory("memory after navigation");
        crate::profile::report_totals();
        capture(&dir, "perf-end");
        let _ = slint::quit_event_loop();
    });
}

/// Search latency, typed-query by typed-query: the library query alone,
/// and the whole search page update.
fn perf_search(ui: &Rc<Ui>) {
    const QUERIES: &[&str] = &[
        "a",
        "b",
        "m",
        "s",
        "bl",
        "ro",
        "an",
        "el",
        "blue",
        "rain",
        "anna",
        "moreau",
        "jazz",
        "golden",
        "sil",
        "north",
        "quartet",
        "deluxe",
        "wint",
        "echo",
        "blue rain",
        "anna mor",
        "velvet garden",
        "classical",
        "fug",
        "orchestra",
        "hidden river",
        "zoé",
        "björn",
        "chlo",
        "station",
        "machine",
        "lunar tide",
        "open room",
        "slow waltz",
        "paper",
        "kowalski",
        "trio",
        "last letters",
        "early season",
    ];
    let mut lib_samples = Vec::new();
    let mut page_samples = Vec::new();
    for q in QUERIES {
        let t = Instant::now();
        let _ = ui.ctx.lib.search(q);
        lib_samples.push(t.elapsed());
        let t = Instant::now();
        ui.app().set_search_text((*q).into());
        crate::views::run_search(ui, q);
        page_samples.push(t.elapsed());
    }
    for (what, s) in [
        ("search (library)", &mut lib_samples),
        ("search (page)", &mut page_samples),
    ] {
        let (p50, p95) = ricercar_core::profile::percentiles(s);
        crate::profile::note(
            what,
            format!(
                "p50 {:.1} ms, p95 {:.1} ms over {} queries",
                p50.as_secs_f64() * 1e3,
                p95.as_secs_f64() * 1e3,
                s.len()
            ),
        );
    }
}

// ------------------------------------------------------------------ plugins tour

/// `RICERCAR_SNAPSHOT_TOUR=plugins`: declares the reference plugin
/// (`ricercar-demo-plugin`, next to this binary; build it with
/// `cargo build -p ricercar-core --bin ricercar-demo-plugin`), then captures
/// the sign-in dialog, the browse pages, a playing plugin track and the
/// plugin search tab.
fn plugins_tour(dir: std::path::PathBuf) {
    let bin = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ricercar-demo-plugin")));
    let Some(bin) = bin.filter(|b| b.exists()) else {
        eprintln!("snapshot: ricercar-demo-plugin not found next to ricercar; build it first");
        let _ = slint::quit_event_loop();
        return;
    };
    with_ui(|ui| {
        ui.ctx.update_config(|c| {
            c.plugins = vec![ricercar_core::config::PluginConfig {
                id: "demo".into(),
                command: bin.clone(),
                args: Vec::new(),
                enabled: true,
            }];
        })
    });
    let album = |r: &str, t: &str| crate::plugins::browse_arg("demo", r, t);
    let steps: Vec<(&str, Step)> = vec![
        (
            "plugin-sign-in",
            (
                2500,
                Box::new(|ui| {
                    // A run on the same XDG dirs may still be signed in.
                    let _ = ui.ctx.plugins.auth_sign_out("demo");
                    crate::plugins::begin_sign_in(ui, "demo");
                    ui.app().set_signin_input("DEMO".into());
                }),
            ),
        ),
        (
            "plugin-browse",
            (
                1200,
                Box::new(move |ui| {
                    crate::plugins::complete_sign_in(ui, "DEMO");
                    ui.navigate(Page::Browse, &album("albums", "Albums"), true);
                }),
            ),
        ),
        (
            "plugin-album",
            (
                1500,
                Box::new(move |ui| {
                    ui.navigate(Page::Browse, &album("album/2", "Night Studies"), true)
                }),
            ),
        ),
        (
            "plugin-signal-path",
            (
                1500,
                Box::new(|ui| {
                    let tracks = ui
                        .st
                        .borrow()
                        .lists
                        .get("browse")
                        .cloned()
                        .unwrap_or_default();
                    let infos: Vec<ricercar_core::TrackInfo> =
                        tracks.iter().map(ricercar_core::TrackInfo::from).collect();
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_tracks(infos, 0, ricercar_core::PlayContext::None);
                    slint::Timer::single_shot(Duration::from_millis(400), move || ctl.pause());
                    let app = ui.app();
                    app.set_np_tab(2);
                    app.set_now_playing_open(true);
                }),
            ),
        ),
        (
            "plugin-search",
            (
                1200,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_now_playing_open(false);
                    app.set_search_text("blue".into());
                    ui.navigate(Page::Search, "", true);
                    app.set_search_source(1);
                    crate::views::run_search(ui, "blue");
                }),
            ),
        ),
    ];
    let mut delay = 0u64;
    let dir = Rc::new(dir);
    let n = steps.len();
    for (i, (name, (wait, f))) in steps.into_iter().enumerate() {
        delay += wait;
        let dir = dir.clone();
        let f = Rc::new(f);
        slint::Timer::single_shot(Duration::from_millis(delay), move || with_ui(|ui| f(ui)));
        delay += 1600;
        let name = name.to_string();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            with_ui(|ui| ui.refill_covers());
            capture(&dir, &name);
            if i + 1 == n {
                with_ui(|ui| ui.ctx.plugins.shutdown());
                let _ = slint::quit_event_loop();
            }
        });
    }
}
