//! Source plugins (docs/plugins.md): separate processes speaking JSON-RPC 2.0
//! over stdio that bring catalogues and turn their items into playable URLs.
//! The host never contains service-specific code or credentials.

pub mod catalog;
mod host;
mod rpc;

use serde::{Deserialize, Serialize};

use crate::controller::TrackInfo;

pub use host::{PluginHost, PluginStatus, RunState};

/// Protocol version spoken by this host.
pub const PROTOCOL: u32 = 1;

/// What the active output accepts natively (sent in `initialize` and
/// `output.changed`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub device: String,
    pub bit_perfect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bits: Option<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rates: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub auth: bool,
    pub browse: bool,
    pub search: bool,
    pub resolve: bool,
    pub favorites: bool,
    pub reporting: bool,
    pub remote_control: bool,
    /// Albums, artists and tracks join the host's own pages.
    pub library: bool,
}

/// The lists of the `library` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibraryList {
    Albums,
    Artists,
    Tracks,
    /// Optional: the user's playlists on the service.
    Playlists,
}

impl LibraryList {
    pub fn method(self) -> &'static str {
        match self {
            LibraryList::Albums => "library.albums",
            LibraryList::Artists => "library.artists",
            LibraryList::Tracks => "library.tracks",
            LibraryList::Playlists => "library.playlists",
        }
    }
}

/// Most items read per library list and plugin.
pub const LIBRARY_MAX: usize = 20_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub display_name: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    #[default]
    SignedOut,
    SignedIn,
    Expired,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthStatus {
    pub state: AuthState,
    pub account: Option<Account>,
}

/// Answer to `auth.begin`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthBegin {
    pub url: String,
    pub instructions: Option<String>,
    pub expects_input: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Format {
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub channels: Option<u8>,
    pub codec: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    #[default]
    Track,
    Album,
    Artist,
    Playlist,
    Folder,
}

/// An entry returned by a plugin (browse, search, item.get).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: ItemKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_ms: Option<u64>,
    pub art: Option<String>,
    pub format: Option<Format>,
    pub playable: Option<bool>,
    pub browsable: Option<bool>,
}

/// Longest `ref` accepted from a plugin.
pub const MAX_REF: usize = 1024;

impl Item {
    pub fn is_playable(&self) -> bool {
        self.kind == ItemKind::Track && self.playable != Some(false)
    }

    pub fn is_browsable(&self) -> bool {
        self.browsable
            .unwrap_or(!matches!(self.kind, ItemKind::Track))
    }

    /// Queue entry for a track item of plugin `id`.
    pub fn to_track_info(&self, id: &str) -> TrackInfo {
        let f = self.format.clone().unwrap_or_default();
        TrackInfo {
            uri: plugin_uri(id, &self.reference),
            path: None,
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            album_artist: self.album_artist.clone(),
            album_id: None,
            duration_ms: self.duration_ms.unwrap_or(0),
            cover: self
                .art
                .clone()
                .filter(|a| a.starts_with("http://") || a.starts_with("https://")),
            track_no: self.track_no,
            year: self.year,
            genre: self.genre.clone(),
            sample_rate: f.sample_rate,
            bits: f.bits,
            codec: f.codec.map(|c| c.to_uppercase()),
            ..Default::default()
        }
    }
}

/// Answer to `track.resolve`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resolved {
    pub url: String,
    pub expires_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub format: Option<Format>,
    pub replaygain: Option<ReplayGainInfo>,
    pub live: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayGainInfo {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

impl Resolved {
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    /// Only http(s) and file URLs are handed to the engine.
    pub fn url_is_acceptable(&self) -> bool {
        ["http://", "https://", "file://"]
            .iter()
            .any(|p| self.url.starts_with(p))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Play,
    Preload,
}

/// Errors from a plugin call, mapped from the protocol codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// -32001: sign in again.
    AuthRequired,
    /// -32002
    NotFound,
    /// -32003: region, subscription or format.
    Unavailable,
    /// -32004, with the delay asked for.
    RateLimited { retry_after: u64 },
    /// -32005
    Network,
    /// The plugin is not running (or not declared, or disabled).
    NotRunning,
    /// No answer in time.
    Timeout,
    /// Anything else, with the plugin's code; `message` is plain text.
    Other { code: i64, message: String },
}

impl PluginError {
    pub fn from_code(code: i64, message: &str, retry_after: Option<u64>) -> PluginError {
        match code {
            -32001 => PluginError::AuthRequired,
            -32002 => PluginError::NotFound,
            -32003 => PluginError::Unavailable,
            -32004 => PluginError::RateLimited {
                retry_after: retry_after.unwrap_or(5),
            },
            -32005 => PluginError::Network,
            _ => PluginError::Other {
                code,
                message: message.chars().take(200).collect(),
            },
        }
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::AuthRequired => write!(f, "sign-in required"),
            PluginError::NotFound => write!(f, "not found"),
            PluginError::Unavailable => {
                write!(f, "not available (region, subscription or format)")
            }
            PluginError::RateLimited { retry_after } => {
                write!(f, "too many requests, retry in {retry_after} s")
            }
            PluginError::Network => write!(f, "network error"),
            PluginError::NotRunning => write!(f, "plugin not running"),
            PluginError::Timeout => write!(f, "plugin did not answer in time"),
            PluginError::Other { code, message } => write!(f, "plugin error {code}: {message}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// Turns `plugin://` URIs into playable URLs; implemented by `PluginHost`
/// and used by the `Controller`. Blocking: call it off the UI thread and
/// off the controller lock.
pub trait Resolver: Send + Sync {
    fn resolve(&self, uri: &str, purpose: Purpose) -> Result<Resolved, PluginError>;
    /// Display name of the plugin behind a URI.
    fn plugin_name(&self, uri: &str) -> Option<String>;
}

// ---------------------------------------------------------------- URIs

const SCHEME: &str = "plugin://";

pub fn is_plugin_uri(uri: &str) -> bool {
    uri.starts_with(SCHEME)
}

/// `plugin://<id>/<percent-encoded ref>`.
pub fn plugin_uri(id: &str, reference: &str) -> String {
    let mut out = String::with_capacity(SCHEME.len() + id.len() + reference.len() + 1);
    out.push_str(SCHEME);
    out.push_str(id);
    out.push('/');
    for b in reference.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// (plugin id, ref) of a `plugin://` URI.
pub fn parse_plugin_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(SCHEME)?;
    let (id, reference) = rest.split_once('/')?;
    if !crate::config::PluginConfig::valid_id(id) || reference.is_empty() {
        return None;
    }
    Some((id.to_string(), crate::meta::percent_decode(reference)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_roundtrip() {
        for r in ["track/8812", "a b/ç?#%&=+", "x"] {
            let u = plugin_uri("demo-2", r);
            assert!(is_plugin_uri(&u));
            assert!(!u[SCHEME.len()..].contains(' '));
            assert_eq!(
                parse_plugin_uri(&u),
                Some(("demo-2".to_string(), r.to_string()))
            );
        }
        assert_eq!(parse_plugin_uri("plugin://Bad/x"), None);
        assert_eq!(parse_plugin_uri("plugin://demo/"), None);
        assert_eq!(parse_plugin_uri("file:///x"), None);
    }

    #[test]
    fn items_parse_and_map() {
        let json = r#"{"ref":"track/3","kind":"track","title":"T","artist":"A","album":"Al",
            "track_no":3,"duration_ms":245000,"art":"https://x/c.jpg",
            "format":{"sample_rate":96000,"bits":24,"codec":"flac"},"playable":true,"extra":1}"#;
        let it: Item = serde_json::from_str(json).unwrap();
        assert!(it.is_playable() && !it.is_browsable());
        let info = it.to_track_info("demo");
        assert_eq!(info.uri, "plugin://demo/track%2F3");
        assert_eq!(info.path, None);
        assert_eq!(info.cover.as_deref(), Some("https://x/c.jpg"));
        assert_eq!((info.sample_rate, info.bits), (Some(96000), Some(24)));
        assert_eq!(info.codec.as_deref(), Some("FLAC"));
        let album: Item =
            serde_json::from_str(r#"{"ref":"album/1","kind":"album","title":"X"}"#).unwrap();
        assert!(album.is_browsable() && !album.is_playable());
        let art: Item =
            serde_json::from_str(r#"{"ref":"t","title":"X","art":"file:///etc/passwd"}"#).unwrap();
        assert_eq!(art.to_track_info("d").cover, None);
    }

    #[test]
    fn error_codes() {
        assert_eq!(
            PluginError::from_code(-32001, "", None),
            PluginError::AuthRequired
        );
        assert_eq!(
            PluginError::from_code(-32004, "", Some(9)),
            PluginError::RateLimited { retry_after: 9 }
        );
        assert!(matches!(
            PluginError::from_code(-1, &"x".repeat(500), None),
            PluginError::Other { message, .. } if message.len() == 200
        ));
    }
}
