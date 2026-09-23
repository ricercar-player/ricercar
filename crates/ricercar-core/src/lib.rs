//! ricercar-core — library, queue, controller, metadata.

pub mod controller;
pub mod library;
pub mod meta;
pub mod watcher;

pub use controller::{Controller, CtlState, Origin, TrackInfo};
pub use library::{AlbumKey, Library, Track};
