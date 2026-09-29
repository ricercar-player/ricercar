//! ricercar-core — library, queue, controller, metadata, configuration.

pub mod config;
pub mod controller;
pub mod covers;
pub mod library;
pub mod meta;
pub mod plugin;
pub mod profile;
#[doc(hidden)]
pub mod synth;
pub mod update;
pub mod watcher;

pub use config::Config;
pub use controller::{
    Controller, CtlEvent, CtlState, EnqueueAt, Origin, PlayContext, QueueItem, Repeat, TrackInfo,
};
pub use library::{
    Album, AlbumSort, Artist, Genre, Library, Playlist, RateCount, SearchResults, Track, TrackSort,
};
