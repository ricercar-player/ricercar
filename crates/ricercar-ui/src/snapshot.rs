//! Headless screenshots for development and docs: `RICERCAR_SNAPSHOT=<dir>`
//! renders the window with Slint's software renderer (no display needed),
//! walks through the main views and writes one PNG per view.
//!
//! The tours run in a throwaway home (`isolate_home`): they never read the
//! user's config, library or music folder, and show no personal path.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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

/// The per-user variables of a tour run in the throwaway home `home`:
/// set to a path inside it, or removed (`None`).
pub fn isolated_env(home: &Path) -> Vec<(&'static str, Option<PathBuf>)> {
    vec![
        ("HOME", Some(home.to_path_buf())),
        ("XDG_CONFIG_HOME", Some(home.join(".config"))),
        ("XDG_DATA_HOME", Some(home.join(".local/share"))),
        ("XDG_CACHE_HOME", Some(home.join(".cache"))),
        ("XDG_STATE_HOME", Some(home.join(".local/state"))),
        // Read by some tools in place of user-dirs.dirs.
        ("XDG_MUSIC_DIR", None),
    ]
}

/// With `RICERCAR_SNAPSHOT` set, point `HOME` and the XDG directories to a
/// throwaway home: `RICERCAR_SNAPSHOT_HOME` (kept, for runs that follow
/// each other, like the state tour), else a new temporary directory,
/// returned so the caller removes it at the end. The first `--library`
/// folder (the demo library) becomes its `~/Music`, a link, so the tour
/// shows it as the desktop's music folder.
///
/// # Safety
///
/// Changes the environment: call before any other thread starts.
pub unsafe fn isolate_home(args: &mut ricercar_daemon::Args) -> std::io::Result<Option<PathBuf>> {
    if std::env::var_os("RICERCAR_SNAPSHOT").is_none() {
        return Ok(None);
    }
    let (home, temporary) = match std::env::var_os("RICERCAR_SNAPSHOT_HOME") {
        Some(h) => (std::path::absolute(PathBuf::from(h))?, false),
        None => {
            let h = std::env::temp_dir().join(format!("ricercar-snapshot-{}", std::process::id()));
            if h.exists() {
                std::fs::remove_dir_all(&h)?;
            }
            (h, true)
        }
    };
    std::fs::create_dir_all(&home)?;
    if let Some(root) = args.roots.first_mut() {
        let music = home.join("Music");
        let target = std::fs::canonicalize(&*root)?;
        if std::fs::symlink_metadata(&music).is_ok() {
            std::fs::remove_file(&music)?;
        }
        std::os::unix::fs::symlink(target, &music)?;
        *root = music;
    }
    for (key, value) in isolated_env(&home) {
        match value {
            // SAFETY: the caller guarantees no other thread runs yet.
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
    Ok(temporary.then_some(home))
}

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
pub fn install() -> Result<(), String> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(PhysicalSize::new(W, H));
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    slint::platform::set_platform(Box::new(Headless {
        window,
        start: Instant::now(),
        queue: Queue::default(),
    }))
    .map_err(|e| format!("snapshot mode: {e}"))
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

/// `RICERCAR_SNAPSHOT_TOUR`: the default screenshot tour, `perf`, `state`,
/// `plugins` or `stream`.
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
        "stream" => return stream_tour(dir),
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
    let one_take = album("Live in One Take");
    let fifths = album("Pure Fifths");
    let harbour = album("Harbour Lights");
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
                    app.set_search_text("ensemble".into());
                    app.invoke_search("blue".into());
                }),
            ),
        ),
        (
            "genres",
            (1200, Box::new(|ui| ui.navigate(Page::Genres, "", true))),
        ),
        (
            "composers",
            (1200, Box::new(|ui| ui.navigate(Page::Composers, "", true))),
        ),
        (
            "composer",
            (
                1200,
                Box::new(|ui| ui.navigate(Page::Composer, "Johann Sebastian Bach", true)),
            ),
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
            // Its header's menu (the "…" button), as for an album.
            "playlist-menu",
            (
                300,
                Box::new(|ui| click(ui, LOCAL_PL_MORE.0, LOCAL_PL_MORE.1)),
            ),
        ),
        (
            // The same menu from the keyboard (Enter on "…"): its first
            // line takes the focus, Tab moves on.
            "playlist-menu-keyboard",
            (
                200,
                Box::new(|ui| {
                    close_menu(ui);
                    let tm = ui.window.global::<crate::TrackMenu>();
                    tm.set_album_id(ui.app().get_pl_card());
                    tm.set_album_plugin(false);
                    tm.set_x(LOCAL_PL_MORE.0 - 16.0);
                    tm.set_y(LOCAL_PL_MORE.1 + 16.0);
                    tm.set_keyboard(true);
                    tm.set_album_serial(tm.get_album_serial() + 1);
                    let ui = ui.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        type_text(&ui, "\t\t")
                    });
                }),
            ),
        ),
        (
            // Sorted by length: the sort menu shows the choice.
            "playlist-sorted",
            (
                600,
                Box::new(|ui| {
                    close_menu(ui);
                    ui.app().set_playlist_sort(5);
                    ui.app().invoke_playlist_sort_changed();
                }),
            ),
        ),
        (
            "settings",
            (
                1200,
                Box::new(|ui| {
                    close_menu(ui);
                    ui.app().set_playlist_sort(0);
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
                            dsd: vec!["DSD_U32_BE".into()],
                        };
                        crate::extras::store_caps(ui, &d.name, caps);
                        // DSD sent as DoP to it: the choice and the badge.
                        let name = d.name.clone();
                        ui.ctx.update_config(|c| {
                            c.audio
                                .dsd
                                .insert(name, ricercar_core::config::DsdOutput::Dop);
                        });
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
            // The setup assistant, opened from the settings: the language,
            // the music folders, plugins from a made-up local catalogue
            // (nothing installed), the outputs, the example capabilities
            // above, fixed volume, the summary.
            "setup-language",
            (
                900,
                Box::new(|ui| {
                    ui.app().set_settings_at_end(false);
                    crate::setup::open(ui);
                }),
            ),
        ),
        (
            "setup-library",
            (600, Box::new(|ui| crate::setup::go(ui, 1))),
        ),
        (
            "setup-plugins",
            (
                900,
                Box::new(|ui| {
                    let index = ricercar_core::config::cache_dir().join("setup-index.json");
                    let _ = std::fs::create_dir_all(index.parent().unwrap());
                    let _ = std::fs::write(&index, setup_index().to_string());
                    ui.plugins.borrow_mut().index_override = Some(index.display().to_string());
                    crate::setup::go(ui, 2);
                }),
            ),
        ),
        (
            "setup-plugins-confirm",
            (
                600,
                Box::new(|ui| {
                    ui.app()
                        .invoke_setup_plugin_check("example-source".into(), true);
                    ui.app().invoke_setup_next();
                }),
            ),
        ),
        (
            "setup-output",
            (
                600,
                Box::new(|ui| {
                    ui.plugins.borrow_mut().index_override = None;
                    ui.app().invoke_setup_back();
                    ui.app()
                        .invoke_setup_plugin_check("example-source".into(), false);
                    crate::setup::go(ui, 3);
                }),
            ),
        ),
        (
            "setup-dac",
            (
                700,
                Box::new(|ui| {
                    let hw = ricercar_audio::device::list_devices()
                        .into_iter()
                        .find(|d| d.kind == ricercar_audio::DeviceKind::Hardware);
                    if let Some(d) = hw {
                        ui.app().invoke_setup_pick(d.name.into());
                    }
                    crate::setup::go(ui, 4);
                }),
            ),
        ),
        (
            "setup-volume",
            (
                600,
                Box::new(|ui| {
                    ui.app().set_setup_fixed(true);
                    crate::setup::go(ui, 5);
                }),
            ),
        ),
        ("setup-done", (600, Box::new(|ui| crate::setup::go(ui, 6)))),
        (
            // An album ripped as one file with a cue sheet: its tracks,
            // and the second one playing.
            "cue-album",
            (
                1500,
                Box::new(move |ui| {
                    ui.app().set_setup_open(false);
                    let id = one_take();
                    let tracks = ui.ctx.lib.album_tracks(&id);
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_library_tracks(
                        &tracks,
                        1,
                        ricercar_core::PlayContext::Album(id.clone()),
                    );
                    ctl.pause();
                    ui.navigate(Page::Album, &id, true);
                    slint::Timer::single_shot(Duration::from_millis(300), move || {
                        ctl.seek_ms(5_000);
                    });
                }),
            ),
        ),
        (
            "cue-signal-path",
            (
                900,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_np_tab(2);
                    app.set_now_playing_open(true);
                }),
            ),
        ),
        (
            // A SACD image: its stereo area's tracks, played from the image.
            "sacd-album",
            (
                1500,
                Box::new(move |ui| {
                    let id = harbour();
                    let tracks = ui.ctx.lib.album_tracks(&id);
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_library_tracks(
                        &tracks,
                        1,
                        ricercar_core::PlayContext::Album(id.clone()),
                    );
                    ctl.pause();
                    ui.app().set_now_playing_open(false);
                    ui.navigate(Page::Album, &id, true);
                    slint::Timer::single_shot(Duration::from_millis(300), move || {
                        ctl.seek_ms(3_000);
                    });
                }),
            ),
        ),
        (
            "sacd-signal-path",
            (
                900,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_np_tab(2);
                    app.set_now_playing_open(true);
                }),
            ),
        ),
        (
            // A DSD64 album, converted to PCM on this output (the default).
            "dsd-album",
            (
                1500,
                Box::new(move |ui| {
                    let id = fifths();
                    let tracks = ui.ctx.lib.album_tracks(&id);
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_library_tracks(
                        &tracks,
                        0,
                        ricercar_core::PlayContext::Album(id.clone()),
                    );
                    ctl.pause();
                    ui.app().set_now_playing_open(false);
                    ui.navigate(Page::Album, &id, true);
                    slint::Timer::single_shot(Duration::from_millis(300), move || {
                        ctl.seek_ms(4_000);
                    });
                }),
            ),
        ),
        (
            "dsd-signal-path",
            (
                900,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_np_tab(2);
                    app.set_now_playing_open(true);
                }),
            ),
        ),
        (
            // The same track as DoP (the null sink stands in for a DAC).
            "dsd-dop-signal-path",
            (
                1500,
                Box::new(|ui| {
                    let device = ui.ctx.ctl.device_name();
                    ui.ctx.ctl.set_dsd_modes(
                        [(device, ricercar_audio::dsd::DsdMode::Dop)]
                            .into_iter()
                            .collect(),
                    );
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
                    ui.app().set_now_playing_open(false);
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

// ------------------------------------------------------------------ stream tour

/// `RICERCAR_SNAPSHOT_TOUR=stream` with `RICERCAR_SNAPSHOT_URL=<http URL>`:
/// plays a (slow) HTTP stream and captures the player bar while it loads,
/// plays, after a seek far ahead, and after a seek back into the part not
/// received yet.
fn stream_tour(dir: std::path::PathBuf) {
    let url = std::env::var("RICERCAR_SNAPSHOT_URL").unwrap_or_default();
    let steps: Vec<(&str, Step)> = vec![
        (
            "",
            (
                300,
                Box::new(move |ui| {
                    let info = ricercar_core::TrackInfo {
                        uri: url.clone(),
                        title: "Slow stream".into(),
                        artist: Some("Local relay".into()),
                        ..Default::default()
                    };
                    ui.ctx
                        .ctl
                        .play_tracks(vec![info], 0, ricercar_core::PlayContext::None);
                }),
            ),
        ),
        ("stream-loading", (1200, Box::new(|_| {}))),
        ("stream-playing", (4500, Box::new(|_| {}))),
        (
            "",
            (
                100,
                Box::new(|ui| {
                    let dur = ui.ctx.ctl.lock().dur_ms;
                    ui.ctx.ctl.seek_ms(dur * 3 / 4);
                }),
            ),
        ),
        ("stream-seeking", (1200, Box::new(|_| {}))),
        ("stream-after-seek", (4500, Box::new(|_| {}))),
        (
            "",
            (
                100,
                Box::new(|ui| {
                    let dur = ui.ctx.ctl.lock().dur_ms;
                    ui.ctx.ctl.seek_ms(dur * 2 / 5);
                }),
            ),
        ),
        ("stream-back-into-hole", (1200, Box::new(|_| {}))),
    ];
    let dir = Rc::new(dir);
    let mut delay = 0;
    let n = steps.len();
    for (i, (name, (wait, f))) in steps.into_iter().enumerate() {
        delay += wait;
        let dir = dir.clone();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            with_ui(|ui| f(ui));
            if !name.is_empty() {
                capture(&dir, name);
            }
            if i + 1 == n {
                let _ = slint::quit_event_loop();
            }
        });
    }
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

/// Open the context menu of a track (`list`, `index`) or, with `album`,
/// of the album page, as a right click would.
fn open_menu(ui: &Ui, album: bool, list: &str, index: i32) {
    let tm = ui.window.global::<crate::TrackMenu>();
    tm.set_x(620.0);
    tm.set_y(430.0);
    if album {
        tm.set_album_id(ui.app().get_al_id());
        tm.set_album_plugin(ui.app().get_al_plugin());
        tm.set_album_serial(tm.get_album_serial() + 1);
    } else {
        tm.set_list(list.into());
        tm.set_index(index);
        tm.set_track_plugin(list != "queue");
        tm.set_serial(tm.get_serial() + 1);
    }
}

/// Close an open context menu: a click beside it.
fn close_menu(ui: &Ui) {
    click(ui, 1200.0, 200.0);
}

/// Where the "…" button of a local playlist's header and of a plugin
/// playlist's sit (window coordinates, the page scrolled to the top)…
const LOCAL_PL_MORE: (f32, f32) = (551.0, 314.0);
const PL_MORE: (f32, f32) = (551.0, 167.0);
/// …and the "Rename" and "Delete playlist" lines of the plugin playlist's
/// menu, opened from it.
const PL_RENAME: (f32, f32) = (635.0, 452.0);
const PL_DELETE: (f32, f32) = (635.0, 486.0);

/// A click of the left button at window coordinates.
fn click(ui: &Ui, x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let w = ui.window.window();
    let position = slint::LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    w.dispatch_event(WindowEvent::PointerMoved { position });
    w.dispatch_event(WindowEvent::PointerPressed { position, button });
    w.dispatch_event(WindowEvent::PointerReleased { position, button });
    w.dispatch_event(WindowEvent::PointerExited);
}

/// Keys typed into the focused element.
fn type_text(ui: &Ui, text: &str) {
    use slint::platform::WindowEvent;
    let w = ui.window.window();
    for c in text.chars() {
        let text = slint::SharedString::from(c.to_string());
        w.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.dispatch_event(WindowEvent::KeyReleased { text });
    }
}

/// The context menu of track `index` of `list`, as a right click opens it.
fn open_track_menu(ui: &Ui, list: &str, index: i32, x: f32, y: f32) {
    let m = ui.window.global::<crate::TrackMenu>();
    m.set_list(list.into());
    m.set_index(index);
    m.set_track_plugin(true);
    m.set_x(x);
    m.set_y(y);
    m.set_serial(m.get_serial() + 1);
}

/// `RICERCAR_SNAPSHOT_TOUR=plugins`: declares the reference plugin
/// (`ricercar-demo-plugin`, next to this binary; build it with
/// `cargo build -p ricercar-core --bin ricercar-demo-plugin`), then captures
/// the sign-in dialog, the browse pages, a playing plugin track, the
/// plugin search tab and the plugin's settings dialog.
/// Scroll the page under the middle of the window down by `dy` pixels.
fn scroll_page(ui: &Ui, dy: f32) {
    use slint::platform::WindowEvent;
    ui.window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(700.0, 400.0),
            delta_x: 0.0,
            delta_y: -dy,
        });
}

/// A made-up catalogue for the setup assistant: two entries whose
/// binaries are never downloaded (the tour stops at the confirmation).
fn setup_index() -> serde_json::Value {
    let entry = |id: &str, name: &str, description: &str, caps: &[&str]| {
        serde_json::json!({
            "id": id,
            "name": name,
            "description": description,
            "repository": format!("https://example.org/{id}"),
            "author": "Example author",
            "license": "MIT",
            "version": "1.0.0",
            "protocol": ricercar_core::plugin::PROTOCOL,
            "capabilities": caps,
            "assets": [{
                "arch": ricercar_core::plugin::catalog::current_arch(),
                "url": format!("https://example.org/{id}/releases/{id}"),
                "sha256": "0".repeat(64),
            }]
        })
    };
    serde_json::json!({"version": 1, "plugins": [
        entry("example-source", "Example source", "Albums and playlists from an example service, after you sign in.", &["auth", "browse", "search", "resolve"]),
        entry("example-radio", "Example radio", "Stations from an example directory.", &["browse", "resolve"]),
    ]})
}

fn plugins_tour(dir: std::path::PathBuf) {
    let bin = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ricercar-demo-plugin")));
    let Some(bin) = bin.filter(|b| b.exists()) else {
        eprintln!("snapshot: ricercar-demo-plugin not found next to ricercar; build it first");
        let _ = slint::quit_event_loop();
        return;
    };
    // A local catalogue whose one entry points at the demo binary.
    let bytes = std::fs::read(&bin).unwrap_or_default();
    let index = serde_json::json!({"version": 1, "plugins": [{
        "id": "demo",
        "name": "Demo Music",
        "description": "Reference plugin: a few generated tracks served from this computer. Sign-in code: DEMO.",
        "repository": "https://github.com/ricercar-player/ricercar",
        "author": "ricercar",
        "license": "MIT",
        "version": "1.0.0",
        "protocol": 1,
        "capabilities": ["auth", "browse", "search", "resolve", "favorites"],
        "assets": [{
            "arch": ricercar_core::plugin::catalog::current_arch(),
            "url": format!("file://{}", bin.display()),
            "sha256": ricercar_core::plugin::catalog::sha256_hex(&bytes),
        }]
    }]});
    let index_path = dir.join("plugin-index.json");
    let _ = std::fs::write(&index_path, index.to_string());
    with_ui(|ui| {
        ui.plugins.borrow_mut().index_override = Some(index_path.display().to_string());
    });
    let file_bin = bin.clone();
    let album = |r: &str, t: &str| crate::plugins::browse_arg("demo", r, t);
    let steps: Vec<(&str, Step)> = vec![
        (
            "plugins-catalogue",
            (1500, Box::new(|ui| ui.navigate(Page::Plugins, "", true))),
        ),
        (
            "plugins-add-link",
            (
                900,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_link_value("https://github.com/someone/ricercar-example".into());
                    app.set_link_open(true);
                }),
            ),
        ),
        (
            "plugins-add-file",
            (
                900,
                Box::new(move |ui| {
                    ui.app().set_link_open(false);
                    if let Ok(c) = ricercar_core::plugin::source::read_file(&file_bin) {
                        crate::plugins::confirm_binary(ui, c);
                    }
                }),
            ),
        ),
        (
            "plugins-install",
            (
                900,
                Box::new(|ui| crate::plugins::confirm_install(ui, "demo", false)),
            ),
        ),
        (
            "plugins-installed",
            (
                3000,
                Box::new(|ui| {
                    crate::plugins::do_install(ui);
                }),
            ),
        ),
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
            "library-albums",
            (1500, Box::new(|ui| ui.navigate(Page::Albums, "", true))),
        ),
        (
            "library-artist",
            (
                1200,
                Box::new(|ui| ui.navigate(Page::Artist, "Demo Ensemble", true)),
            ),
        ),
        (
            "plugin-browse-artist",
            (
                1500,
                Box::new(move |ui| {
                    ui.navigate(Page::Browse, &album("artist/1", "Demo Ensemble"), true)
                }),
            ),
        ),
        (
            "library-tracks",
            (1200, Box::new(|ui| ui.navigate(Page::Tracks, "", true))),
        ),
        (
            "plugin-album",
            (
                1500,
                Box::new(move |ui| {
                    let id = format!(
                        "{}{}",
                        crate::plugins::ALBUM_CARD,
                        album("album/2", "Night Studies")
                    );
                    ui.navigate(Page::Album, &id, true)
                }),
            ),
        ),
        (
            // Its heart, filled at once (favourite state of the plugin).
            "plugin-album-fav",
            (
                200,
                Box::new(|ui| {
                    let app = ui.app();
                    app.invoke_album_action(app.get_al_id(), "fav".into());
                }),
            ),
        ),
        (
            // Go to album / artist and the plugin's actions on a track.
            "plugin-track-menu",
            (600, Box::new(|ui| open_menu(ui, false, "album", 0))),
        ),
        (
            // The album's own menu (More button): its actions.
            "plugin-album-menu",
            (
                200,
                Box::new(|ui| {
                    close_menu(ui);
                    open_menu(ui, true, "", 0);
                }),
            ),
        ),
        (
            "plugin-signal-path",
            (
                1500,
                Box::new(|ui| {
                    close_menu(ui);
                    let tracks = ui
                        .st
                        .borrow()
                        .lists
                        .get("album")
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
            // A queue entry's menu, in the now-playing view.
            "plugin-queue-menu",
            (
                300,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_np_tab(1);
                    app.set_now_playing_open(true);
                    // The album's tracks, with their refs, in the queue.
                    let tracks = ui
                        .st
                        .borrow()
                        .lists
                        .get("album")
                        .cloned()
                        .unwrap_or_default();
                    let infos = tracks.iter().map(ricercar_core::TrackInfo::from).collect();
                    let ctl = ui.ctx.ctl.clone();
                    ctl.play_tracks(infos, 0, ricercar_core::PlayContext::None);
                    let first = ctl.lock().queue.first().map(|q| q.id);
                    slint::Timer::single_shot(Duration::from_millis(400), move || ctl.pause());
                    if let Some(id) = first {
                        open_menu(ui, false, "queue", id as i32);
                    }
                }),
            ),
        ),
        (
            "plugin-now-album",
            (
                600,
                Box::new(|ui| {
                    close_menu(ui);
                    let app = ui.app();
                    app.set_now_playing_open(false);
                    // The bar's title of a playing plugin track.
                    app.invoke_go_to_album();
                }),
            ),
        ),
        (
            // Lyrics given by the plugin (its first album).
            "plugin-lyrics",
            (
                600,
                Box::new(move |ui| {
                    let id = format!(
                        "{}{}",
                        crate::plugins::ALBUM_CARD,
                        album("album/1", "Demo Sessions")
                    );
                    ui.navigate(Page::Album, &id, true);
                    slint::Timer::single_shot(Duration::from_millis(700), || {
                        with_ui(|ui| {
                            // Only the track with synced lyrics: the demo
                            // tracks last a second.
                            let first: Vec<ricercar_core::TrackInfo> = ui
                                .st
                                .borrow()
                                .lists
                                .get("album")
                                .and_then(|l| l.iter().find(|t| t.title == "Opening"))
                                .map(ricercar_core::TrackInfo::from)
                                .into_iter()
                                .collect();
                            let ctl = ui.ctx.ctl.clone();
                            ctl.play_tracks(first, 0, ricercar_core::PlayContext::None);
                            slint::Timer::single_shot(Duration::from_millis(200), move || {
                                ctl.pause()
                            });
                            let app = ui.app();
                            app.set_np_tab(0);
                            app.set_now_playing_open(true);
                        })
                    });
                }),
            ),
        ),
        (
            "plugin-artist",
            (400, Box::new(|ui| ui.app().invoke_go_to_artist())),
        ),
        (
            // A plugin artist (its artist_ref) with its actions menu.
            "plugin-artist-menu",
            (
                1200,
                Box::new(move |ui| {
                    let tm = ui.window.global::<crate::TrackMenu>();
                    tm.set_album_id(ui.app().get_br_menu_id());
                    tm.set_album_plugin(true);
                    tm.set_x(300.0);
                    tm.set_y(200.0);
                    tm.set_album_serial(tm.get_album_serial() + 1);
                }),
            ),
        ),
        (
            "plugin-home",
            (
                600,
                Box::new(|ui| {
                    close_menu(ui);
                    ui.app().set_now_playing_open(false);
                    ui.navigate(Page::Home, "", true);
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
                    app.set_search_text("ensemble".into());
                    ui.navigate(Page::Search, "", true);
                    crate::views::run_search(ui, "ensemble");
                }),
            ),
        ),
        (
            "plugin-search-playlists",
            (200, Box::new(|ui| ui.app().set_search_kind(4))),
        ),
        (
            "plugin-search-library",
            (
                200,
                Box::new(|ui| {
                    let app = ui.app();
                    app.set_search_kind(0);
                    app.set_search_scope(1);
                    app.invoke_search_scope_changed();
                }),
            ),
        ),
        // Playlist editing on the service (the demo's "Demo mix" is the
        // user's, "Followed picks" is followed).
        (
            "plugin-playlist",
            (
                600,
                Box::new(move |ui| {
                    ui.app().set_search_scope(0);
                    if ui.ctx.lib.playlists().is_empty() {
                        ui.ctx.lib.create_playlist("Late night");
                        crate::views::refresh_playlists(ui);
                    }
                    ui.navigate(Page::Browse, &album("playlist/1", "Demo mix"), true);
                }),
            ),
        ),
        (
            // The playlist's own menu: the album entries, rename and delete.
            "plugin-playlist-header-menu",
            (200, Box::new(|ui| click(ui, PL_MORE.0, PL_MORE.1))),
        ),
        (
            "plugin-playlist-menu",
            (
                200,
                Box::new(|ui| {
                    close_menu(ui);
                    open_track_menu(ui, "browse", 1, 700.0, 300.0)
                }),
            ),
        ),
        (
            "plugin-playlist-moved",
            (
                200,
                Box::new(|ui| {
                    // Dismiss the menu, then move the first entry down.
                    click(ui, 1400.0, 880.0);
                    crate::plugins::entry_action(ui, 0, "pl-down");
                }),
            ),
        ),
        (
            "plugin-add-to-playlist",
            (
                200,
                Box::new(|ui| {
                    open_track_menu(ui, "browse", 0, 700.0, 300.0);
                    // "Add to playlist", the fourth line of a track's menu.
                    let ui = ui.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        click(&ui, 760.0, 300.0 + 5.0 + 3.0 * 34.0 + 17.0)
                    });
                }),
            ),
        ),
        (
            "plugin-new-playlist",
            (
                200,
                Box::new(|ui| {
                    // "New playlist…", under "Back" and a separator.
                    click(ui, 760.0, 300.0 + 5.0 + 34.0 + 9.0 + 17.0);
                    let ui = ui.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        ui.app().set_new_pl_location(1);
                        type_text(&ui, "Road trip");
                    });
                }),
            ),
        ),
        (
            "plugin-playlist-created",
            (
                200,
                // Create it on the service with the track.
                Box::new(|ui| type_text(ui, &char::from(slint::platform::Key::Return).to_string())),
            ),
        ),
        (
            "plugin-playlist-new-page",
            (
                200,
                Box::new(move |ui| {
                    ui.navigate(Page::Browse, &album("playlist/3", "Road trip"), true)
                }),
            ),
        ),
        (
            "plugin-playlist-renamed",
            (
                200,
                Box::new(|ui| {
                    click(ui, PL_MORE.0, PL_MORE.1);
                    let ui = ui.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        click(&ui, PL_RENAME.0, PL_RENAME.1);
                        slint::Timer::single_shot(Duration::from_millis(200), move || {
                            let enter = char::from(slint::platform::Key::Return);
                            let end = char::from(slint::platform::Key::End);
                            type_text(&ui, &format!("{end} 2{enter}"));
                        });
                    });
                }),
            ),
        ),
        (
            "plugin-playlist-delete",
            (
                200,
                Box::new(|ui| {
                    click(ui, PL_MORE.0, PL_MORE.1);
                    let ui = ui.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        click(&ui, PL_DELETE.0, PL_DELETE.1)
                    });
                }),
            ),
        ),
        (
            "plugin-playlist-deleted",
            (
                200,
                // From Cancel (focused first) to Delete.
                Box::new(|ui| {
                    let enter = char::from(slint::platform::Key::Return);
                    type_text(ui, &format!("\t{enter}"));
                }),
            ),
        ),
        (
            "plugin-playlist-followed",
            (
                200,
                Box::new(move |ui| {
                    ui.navigate(Page::Browse, &album("playlist/2", "Followed picks"), true);
                }),
            ),
        ),
        (
            // Its header's "Add to queue": every track, read from the plugin.
            "plugin-playlist-queued",
            (
                200,
                Box::new(|ui| {
                    let app = ui.app();
                    app.invoke_album_action(app.get_br_menu_id(), "queue".into());
                    app.set_queue_open(true);
                }),
            ),
        ),
        (
            "plugin-artist-details",
            (
                600,
                Box::new(|ui| {
                    ui.app().set_queue_open(false);
                    ui.navigate(Page::Artist, "Demo Ensemble", true)
                }),
            ),
        ),
        (
            "plugin-artist-related",
            (200, Box::new(|ui| scroll_page(ui, 560.0))),
        ),
        (
            // An artist known only from the plugin: its header's "Add to
            // queue" gathers the tracks of its plugin albums.
            "plugin-artist-queued",
            (
                200,
                Box::new(|ui| {
                    scroll_page(ui, -560.0);
                    let app = ui.app();
                    app.invoke_album_action(app.get_ar_name(), "artist-queue".into());
                    app.set_queue_open(true);
                }),
            ),
        ),
        (
            "plugin-album-details",
            (
                1200,
                Box::new(move |ui| {
                    ui.app().set_queue_open(false);
                    let id = format!(
                        "{}{}",
                        crate::plugins::ALBUM_CARD,
                        album("album/1", "Demo Sessions")
                    );
                    ui.navigate(Page::Album, &id, true)
                }),
            ),
        ),
        (
            "plugin-radio-menu",
            (
                600,
                Box::new(|ui| {
                    // The context menu of the album's first track.
                    let m = ui.window.global::<crate::TrackMenu>();
                    m.set_list("album".into());
                    m.set_index(0);
                    m.set_track_plugin(true);
                    m.set_x(900.0);
                    m.set_y(420.0);
                    m.set_serial(m.get_serial() + 1);
                }),
            ),
        ),
        (
            "plugin-radio-queue",
            (
                300,
                Box::new(|ui| {
                    // "Start radio" in that menu: a click outside closes it.
                    use slint::platform::{PointerEventButton, WindowEvent};
                    let w = ui.window.window();
                    let position = slint::LogicalPosition::new(400.0, 700.0);
                    let button = PointerEventButton::Left;
                    w.dispatch_event(WindowEvent::PointerPressed { position, button });
                    w.dispatch_event(WindowEvent::PointerReleased { position, button });
                    // The toast counts the tracks queued (the demo's
                    // one-second tracks are over by the capture).
                    crate::views::track_action(ui, "album", 0, "radio");
                    ui.app().set_queue_open(true);
                }),
            ),
        ),
        (
            "settings-playback",
            (
                600,
                Box::new(|ui| {
                    ui.app().set_queue_open(false);
                    ui.navigate(Page::Settings, "", true);
                    // Down to the end of "Audio output", past the devices.
                    slint::Timer::single_shot(Duration::from_millis(400), || {
                        with_ui(|ui| scroll_page(ui, 520.0))
                    });
                }),
            ),
        ),
        (
            "plugin-settings",
            (
                600,
                Box::new(|ui| {
                    ui.app().set_search_scope(0);
                    ui.navigate(Page::Plugins, "", true);
                    crate::plugin_settings::open(ui, "demo");
                }),
            ),
        ),
        (
            "plugin-settings-error",
            (
                200,
                Box::new(|ui| {
                    // A value out of range, typed then confirmed.
                    use slint::Model;
                    let rows = ui.app().get_psettings_rows();
                    if let Some(i) = (0..rows.row_count())
                        .find(|i| rows.row_data(*i).unwrap().key == "page_size")
                    {
                        let mut r = rows.row_data(i).unwrap();
                        r.text = "500".into();
                        rows.set_row_data(i, r);
                    }
                    ui.app()
                        .invoke_psettings_text("page_size".into(), "500".into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_user_directory_moves_into_the_throwaway_home() {
        let home = Path::new("/tmp/tour-home");
        let env = isolated_env(home);
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        for k in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_MUSIC_DIR",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
        for (k, v) in &env {
            match v {
                Some(p) => assert!(p.starts_with(home), "{k}"),
                None => assert_eq!(*k, "XDG_MUSIC_DIR"),
            }
        }
    }
}
