//! ricercar desktop app (Slint).

slint::include_modules!();

mod app;
mod extras;
mod images;
mod player;
mod snapshot;
mod text;
mod tray;
mod views;

pub use app::run;
