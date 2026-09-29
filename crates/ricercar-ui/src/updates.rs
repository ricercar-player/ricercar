//! Update notices: a new ricercar release (checked at most once a day) in
//! the sidebar, and a count of plugin updates on the Plugins entry. Both
//! follow Settings → Online extras → Updates; the headless tours never
//! check.

use std::path::PathBuf;
use std::rc::Rc;

use ricercar_core::update::{self, UpdateState};

use crate::app::{Ui, post, with_ui};

fn state_path() -> PathBuf {
    ricercar_core::config::cache_dir().join("update.json")
}

/// At startup: look for a new release and for plugin updates.
pub fn check(ui: &Rc<Ui>) {
    if std::env::var_os("RICERCAR_SNAPSHOT").is_some()
        || !ui.ctx.config.read().unwrap().online.updates
    {
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
        let newer = st.newer_than(env!("CARGO_PKG_VERSION")).cloned();
        if let Some(r) = newer {
            post(move |ui| {
                let app = ui.app();
                app.set_update_version(r.version.into());
                app.set_update_url(r.url.into());
            });
        }
    });
}

pub fn wire(ui: &Rc<Ui>) {
    let app = ui.app();
    app.on_update_open(|| {
        with_ui(|ui| crate::plugins::open_url(&ui.app().get_update_url()));
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
