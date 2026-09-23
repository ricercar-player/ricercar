//! ricercar-audio — bit-perfect PCM pipeline.
//!
//! Decodes audio (symphonia) to interleaved i32 and hands it to a sink
//! (ALSA / null / file) configured at the *exact* track format: the pipeline
//! never resamples, never mixes, never truncates. If a device cannot play a
//! track at its native rate/depth, it is refused, not degraded.

pub mod device;
pub mod error;
pub mod fmt;
pub mod player;
pub mod sink;
pub mod stream;

pub use device::{DeviceInfo, DeviceKind};
pub use error::{AudioError, Result};
pub use fmt::{Container, PcmFormat};
pub use player::{
    spawn_player, ChainInfo, EngineEvent, PlayerHandle, PlayerShared, Subscriber,
    TransportStatus,
};
pub use sink::AudioSink;
pub use stream::TrackSource;
