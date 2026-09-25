//! Online integrations for ricercar, built exclusively on open, documented
//! public APIs. This crate never talks to a streaming service or any other streaming
//! service's private API, and bundles no API keys: every credential (Last.fm
//! API key/secret, ListenBrainz token) is supplied by the user via config.
//!
//! All network calls are blocking (`ureq`), send the ricercar `User-Agent`
//! and use bounded timeouts (connect 8 s, read 15 s); run them off the audio
//! and UI threads.
//!
//! Modules:
//! - [`scrobble`]: service-agnostic scrobble rules ([`scrobble::ScrobbleTracker`])
//!   and a persistent offline queue ([`scrobble::ScrobbleQueue`]).
//! - [`listenbrainz`]: ListenBrainz submission (token auth).
//! - [`lastfm`]: Last.fm desktop auth flow, now playing and scrobbling.
//! - [`lyrics`]: LRC parsing, LRCLIB lookup and an on-disk lyrics cache.
//! - [`radio`]: Radio Browser station directory.
//! - [`coverart`]: MusicBrainz release lookup + Cover Art Archive images,
//!   with a process-wide MusicBrainz rate limiter.
//! - [`error`]: the [`OnlineError`] type shared by every module.

pub mod coverart;
pub mod error;
mod fsutil;
mod http;
pub mod lastfm;
pub mod listenbrainz;
pub mod lyrics;
pub mod radio;
pub mod scrobble;

pub use error::{OnlineError, Result};
pub use http::USER_AGENT;
