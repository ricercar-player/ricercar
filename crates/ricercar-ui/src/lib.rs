//! ricercar desktop app (Slint).

slint::include_modules!();

mod app;
mod dac;
mod diag;
mod extras;
mod images;
mod merge;
mod player;
mod plugins;
mod profile;
mod snapshot;
mod text;
mod tray;
mod ui_state;
mod views;

pub use app::run;
