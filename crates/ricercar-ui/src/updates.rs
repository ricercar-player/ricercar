//! Update notices: a new ricercar release (checked at most once a day) in
//! the sidebar, and a count of plugin updates on the Plugins entry. Both
//! follow Settings → Online extras → Updates; the headless tours never
//! check. Settings → About checks on demand and installs the release
//! (AppImage replaced in place, packages through `pkexec`), then restarts.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use ricercar_core::update::{self, InstallError, Release, UpdateState};

use crate::Page;
use crate::app::{Ui, post, with_ui};
use crate::text::t;

// About → Updates, as `App.update-stage`.
const IDLE: i32 = 0;
const CHECKING: i32 = 1;
const UP_TO_DATE: i32 = 2;
const AVAILABLE: i32 = 3;
const INSTALLING: i32 = 4;
const INSTALLED: i32 = 5;
const FAILED: i32 = 6;

thread_local! {
    /// The newer release on offer.
    static LATEST: RefCell<Option<Release>> = const { RefCell::new(None) };
}

/// Set once an update is installed and the user asked to restart: started
/// by [`restart_if_asked`] after the window is gone.
static RESTART: Mutex<Option<PathBuf>> = Mutex::new(None);

fn state_path() -> PathBuf {
    ricercar_core::config::cache_dir().join("update.json")
}

fn snapshot_mode() -> bool {
    std::env::var_os("RICERCAR_SNAPSHOT").is_some()
}

/// At startup: look for a new release and for plugin updates.
pub fn check(ui: &Rc<Ui>) {
    if snapshot_mode() || !ui.ctx.config.read().unwrap().online.updates {
        return;
    }
    crate::plugins::check_updates(ui);
    std::thread::spawn(|| {
        let path = state_path();
        let mut st = UpdateState::load(&path);
        match update::check(&mut st, ricercar_core::library::now_unix()) {
            Ok(()) => {
                if let Err(e) = st.save(&path) {
                    tracing::warn!("update state {}: {e}", path.display());
                }
            }
            Err(e) => tracing::info!("update check: {e}"),
        }
        let notice = st.newer_than(env!("CARGO_PKG_VERSION")).is_some();
        offer(newer(&st), notice);
    });
}

/// The known release when it is newer than this ricercar, dismissed or not.
fn newer(st: &UpdateState) -> Option<Release> {
    st.latest
        .clone()
        .filter(|r| update::compare(&r.version, env!("CARGO_PKG_VERSION")).is_gt())
}

/// From a worker thread: show `release` in About (and in the sidebar when
/// `notice`), or say ricercar is up to date.
fn offer(release: Option<Release>, notice: bool) {
    let direct = release
        .as_ref()
        .is_some_and(|r| update::installable(r, update::install_kind()));
    post(move |ui| {
        let app = ui.app();
        let Some(r) = release else {
            if app.get_update_stage() == CHECKING {
                app.set_update_stage(UP_TO_DATE);
            }
            return;
        };
        app.set_update_latest(r.version.clone().into());
        app.set_update_direct(direct);
        if notice {
            app.set_update_version(r.version.clone().into());
        }
        app.set_update_url(r.url.clone().into());
        if app.get_update_stage() != INSTALLING && app.get_update_stage() != INSTALLED {
            app.set_update_stage(AVAILABLE);
        }
        LATEST.with(|l| *l.borrow_mut() = Some(r));
    });
}

/// "Check for updates": ask GitHub now, even with the daily check off.
fn check_now(ui: &Rc<Ui>) {
    let app = ui.app();
    app.set_update_stage(CHECKING);
    if snapshot_mode() {
        return;
    }
    std::thread::spawn(|| {
        let path = state_path();
        let mut st = UpdateState::load(&path);
        match update::refresh(&mut st, ricercar_core::library::now_unix()) {
            Ok(()) => {
                if let Err(e) = st.save(&path) {
                    tracing::warn!("update state {}: {e}", path.display());
                }
                offer(newer(&st), false);
            }
            Err(e) => {
                tracing::info!("update check: {e}");
                post(|ui| {
                    fail(
                        ui,
                        t("Could not reach GitHub. Check the connection and try again."),
                    )
                });
            }
        }
    });
}

fn fail(ui: &Ui, msg: &str) {
    let app = ui.app();
    app.set_update_error(msg.into());
    app.set_update_stage(FAILED);
}

/// "Update now": install the release, or open its page when ricercar
/// cannot install it itself.
fn install(ui: &Rc<Ui>) {
    let Some(release) = LATEST.with(|l| l.borrow().clone()) else {
        return;
    };
    let kind = update::install_kind();
    if !update::installable(&release, kind) {
        crate::plugins::open_url(&release.url);
        return;
    }
    let app = ui.app();
    app.set_update_stage(INSTALLING);
    let work = ricercar_core::config::cache_dir().join("update");
    std::thread::spawn(move || {
        let result = update::install(&release, kind, &work);
        post(move |ui| match result {
            Ok(()) => {
                tracing::info!("ricercar {} installed", release.version);
                let app = ui.app();
                app.set_update_version("".into());
                app.set_update_stage(INSTALLED);
            }
            Err(e) => {
                tracing::warn!("update to {}: {e}", release.version);
                let msg = match e {
                    InstallError::Cancelled => t("Update cancelled.").to_string(),
                    InstallError::Checksum => t(
                        "The download does not match the release checksums; nothing was installed.",
                    )
                    .to_string(),
                    InstallError::Network(_) => {
                        t("The download failed. Check the connection and try again.").to_string()
                    }
                    e => format!("{} ({e})", t("The update could not be installed.")),
                };
                fail(ui, &msg);
                // Offer the release page instead.
                ui.app().set_update_direct(false);
            }
        });
    });
}

/// Start the updated ricercar once this one has shut down.
pub fn restart_if_asked() {
    let Some(program) = RESTART.lock().unwrap().take() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&program)
        .args(std::env::args_os().skip(1))
        .exec();
    tracing::warn!("restart {}: {err}", program.display());
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.set_update_stage(IDLE);
    app.on_update_open(|| {
        with_ui(|ui| crate::plugins::open_url(&ui.app().get_update_url()));
    });
    // The sidebar notice leads to Settings → About, where the update is.
    app.on_update_show(|| {
        with_ui(|ui| {
            ui.navigate(Page::Settings, "", true);
            let app = ui.app();
            app.set_settings_at_end(false);
            slint::Timer::single_shot(std::time::Duration::from_millis(50), || {
                with_ui(|ui| ui.app().set_settings_at_end(true));
            });
        })
    });
    app.on_update_check(|| with_ui(check_now));
    app.on_update_install(|| with_ui(install));
    app.on_update_restart(|| {
        if let Some(p) = update::install_kind().program() {
            *RESTART.lock().unwrap() = Some(p.to_path_buf());
            let _ = slint::quit_event_loop();
        }
    });
    app.on_update_dismiss(|| {
        with_ui(|ui| {
            let app = ui.app();
            let version = app.get_update_version().to_string();
            app.set_update_version("".into());
            std::thread::spawn(move || {
                let path = state_path();
                let mut st = UpdateState::load(&path);
                st.dismissed = Some(version);
                let _ = st.save(&path);
            });
        })
    });
}
