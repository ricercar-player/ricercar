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
            let mut scratch = vec![Rgb8Pixel::default(); (W * H) as usize];
            self.window.draw_if_needed(|r| {
                r.render(&mut scratch, W as usize);
            });
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

/// Scripted tour: each step runs after its delay, then the view is captured.
pub fn start(ui: &Rc<Ui>, dir: std::path::PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
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
            (1200, Box::new(|ui| ui.navigate(Page::Settings, "", true))),
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
