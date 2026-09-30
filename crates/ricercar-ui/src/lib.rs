//! ricercar desktop app (Slint).

slint::include_modules!();

mod app;
mod dac;
mod details;
mod diag;
mod extras;
mod images;
mod merge;
mod player;
mod plugin_favs;
mod plugin_menu;
mod plugin_settings;
mod plugins;
mod profile;
mod snapshot;
mod sys;
mod text;
mod tray;
mod ui_state;
mod updates;
mod views;

pub use app::run;
