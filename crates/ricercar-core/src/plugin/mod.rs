//! Source plugins (docs/plugins.md): separate processes speaking JSON-RPC 2.0
//! over stdio that bring catalogues and turn their items into playable URLs.
//! The host never contains service-specific code or credentials.

pub mod catalog;
mod content;
mod host;
mod rpc;
pub mod settings;

use serde::{Deserialize, Serialize};

use crate::controller::TrackInfo;

pub use content::{
    Biography, Fact, ItemDetails, LyricLine, MAX_BIOGRAPHY, MAX_FACTS, MAX_LYRIC_LINES,
    MAX_SHELF_ITEMS, MAX_SHELVES, PluginLyrics, Shelf,
};
pub use host::{PlaylistEdited, PluginHost, PluginStatus, RADIO_LIMIT, RunState, TRACKS_MAX};

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
    /// `lyrics.get`.
    pub lyrics: bool,
    /// `playlists.create`, `.rename`, `.delete`, `.add`, `.remove` (and
    /// optionally `.move`).
    pub playlist_edit: bool,
    /// `item.details`.
    pub details: bool,
    /// `radio.next`.
    pub radio: bool,
}

impl Capabilities {
    /// Names of the capabilities declared, in protocol order.
    pub fn names(&self) -> Vec<&'static str> {
        [
            ("auth", self.auth),
            ("browse", self.browse),
            ("search", self.search),
            ("resolve", self.resolve),
            ("favorites", self.favorites),
            ("reporting", self.reporting),
            ("remote_control", self.remote_control),
            ("library", self.library),
            ("lyrics", self.lyrics),
            ("playlist_edit", self.playlist_edit),
            ("details", self.details),
            ("radio", self.radio),
        ]
        .into_iter()
        .filter_map(|(n, on)| on.then_some(n))
        .collect()
    }
}

/// Every capability name of protocol 1.
pub const CAPABILITY_NAMES: [&str; 12] = [
    "auth",
    "browse",
    "search",
    "resolve",
    "favorites",
    "reporting",
    "remote_control",
    "library",
    "lyrics",
    "playlist_edit",
    "details",
    "radio",
];

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
    /// Albums and playlists: how many tracks they hold.
    pub track_count: Option<u32>,
    pub art: Option<String>,
    pub format: Option<Format>,
    pub playable: Option<bool>,
    pub browsable: Option<bool>,
    /// Tracks: their album, to open in the generic browse page.
    pub album_ref: Option<String>,
    /// Tracks and albums: their artist.
    pub artist_ref: Option<String>,
    /// Albums and tracks: their label or publisher.
    pub label_ref: Option<String>,
    /// Related content offered in the item's menu (at most
    /// [`MAX_ACTIONS`], invalid entries dropped).
    #[serde(deserialize_with = "lenient_actions")]
    pub actions: Vec<Action>,
    /// In the user's favourites on the service; `None`: unknown.
    pub favorite: Option<bool>,
    /// Tracks listed by a playlist's `browse.list`: the entry in that
    /// playlist (a track may appear twice), for `playlists.remove` and
    /// `playlists.move`.
    pub entry_id: Option<String>,
    /// Playlists: the user may edit it (`playlist_edit`).
    pub editable: bool,
}

/// Longest `ref` accepted from a plugin.
pub const MAX_REF: usize = 1024;
/// Most actions kept per item.
pub const MAX_ACTIONS: usize = 8;
const MAX_ACTION_ID: usize = 64;
const MAX_ACTION_LABEL: usize = 80;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Play the tracks of `browse.list(ref)`.
    #[default]
    Play,
    /// Open `ref` in the generic browse page.
    Browse,
}

/// An entry of `Item.actions`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    /// Short text, in the host's locale; plain text.
    pub label: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: ActionKind,
}

impl Action {
    fn valid(&self) -> bool {
        let id = self.id.trim();
        let label = self.label.trim();
        !id.is_empty()
            && id.chars().count() <= MAX_ACTION_ID
            && !label.is_empty()
            && label.chars().count() <= MAX_ACTION_LABEL
            && valid_ref(&self.reference)
    }
}

/// Actions that parse and pass the checks, up to [`MAX_ACTIONS`].
fn lenient_actions<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Action>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.as_ref()
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| serde_json::from_value::<Action>(x.clone()).ok())
                .filter(Action::valid)
                .take(MAX_ACTIONS)
                .collect()
        })
        .unwrap_or_default())
}

/// A ref the host accepts: not empty, at most [`MAX_REF`] bytes.
pub fn valid_ref(r: &str) -> bool {
    !r.is_empty() && r.len() <= MAX_REF
}

impl Item {
    pub fn is_playable(&self) -> bool {
        self.kind == ItemKind::Track && self.playable != Some(false)
    }

    pub fn is_browsable(&self) -> bool {
        self.browsable
            .unwrap_or(!matches!(self.kind, ItemKind::Track))
    }

    /// Drop optional refs that are empty or too long; an `entry_id` too.
    pub(crate) fn sanitize(&mut self) {
        for r in [
            &mut self.album_ref,
            &mut self.artist_ref,
            &mut self.label_ref,
            &mut self.entry_id,
        ] {
            if r.as_deref().is_some_and(|x| !valid_ref(x)) {
                *r = None;
            }
        }
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
            album_ref: self.album_ref.clone(),
            artist_ref: self.artist_ref.clone(),
            ..Default::default()
        }
    }
}

/// How the URL of `track.resolve` delivers the stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Straight from where the service serves it.
    #[default]
    Direct,
    /// Relayed by the plugin itself (for example on 127.0.0.1), the
    /// original codec delivered unchanged.
    Proxied,
}

/// Unknown values read as `direct`.
impl<'de> Deserialize<'de> for Delivery {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Option::<serde_json::Value>::deserialize(d)?;
        Ok(match v.as_ref().and_then(serde_json::Value::as_str) {
            Some("proxied") => Delivery::Proxied,
            _ => Delivery::Direct,
        })
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
    pub delivery: Delivery,
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
    /// The plugin behind `uri` runs and declares `radio`.
    fn has_radio(&self, _uri: &str) -> bool {
        false
    }
    /// `radio.next` seeded by `seed` (a `plugin://` URI): playable tracks
    /// of the seed's plugin only. `exclude`: `plugin://` URIs, those of
    /// other plugins are left out.
    fn radio_next(
        &self,
        _seed: &str,
        _exclude: &[String],
        _limit: usize,
    ) -> Result<Vec<TrackInfo>, PluginError> {
        Err(PluginError::NotRunning)
    }
}

/// Items of a JSON array; entries that do not parse, or with a missing or
/// oversized ref, are dropped.
pub(crate) fn items_of(v: Option<&serde_json::Value>) -> Vec<Item> {
    v.and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| serde_json::from_value::<Item>(i.clone()).ok())
                .filter(|i| valid_ref(&i.reference))
                .map(|mut i| {
                    i.sanitize();
                    i
                })
                .collect()
        })
        .unwrap_or_default()
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
    fn optional_fields_are_checked() {
        let long = "x".repeat(MAX_REF + 1);
        let mut actions: Vec<serde_json::Value> = (0..12)
            .map(|i| serde_json::json!({"id": format!("a{i}"), "label": "L", "ref": "r", "kind": "play"}))
            .collect();
        actions.insert(
            0,
            serde_json::json!({"id": "x", "label": "L", "ref": "r", "kind": "fly"}),
        );
        actions.insert(
            0,
            serde_json::json!({"id": "x", "label": " ", "ref": "r", "kind": "browse"}),
        );
        actions.insert(
            0,
            serde_json::json!({"id": "x", "label": "L", "ref": long, "kind": "browse"}),
        );
        let items = items_of(Some(&serde_json::json!([{
            "ref": "t", "title": "T", "album_ref": "album/1", "artist_ref": "",
            "label_ref": long, "favorite": true, "actions": actions, "entry_id": "e1"
        }, {"ref": "p", "kind": "playlist", "title": "P", "editable": true, "actions": "nope"}])));
        let t = &items[0];
        assert_eq!(t.album_ref.as_deref(), Some("album/1"));
        assert_eq!(
            (t.artist_ref.as_deref(), t.label_ref.as_deref()),
            (None, None)
        );
        assert_eq!(t.favorite, Some(true));
        assert_eq!(t.entry_id.as_deref(), Some("e1"));
        assert_eq!(t.actions.len(), MAX_ACTIONS);
        assert_eq!(t.actions[0].id, "a0");
        assert!(items[1].editable && items[1].actions.is_empty());
        let info = t.to_track_info("demo");
        assert_eq!(
            info.plugin_album_ref(),
            Some(("demo".into(), "album/1".into()))
        );
        assert_eq!(info.plugin_artist_ref(), None);
    }

    #[test]
    fn delivery_defaults_to_direct() {
        let r: Resolved = serde_json::from_str(r#"{"url":"http://x"}"#).unwrap();
        assert_eq!(r.delivery, Delivery::Direct);
        let r: Resolved =
            serde_json::from_str(r#"{"url":"http://x","delivery":"proxied"}"#).unwrap();
        assert_eq!(r.delivery, Delivery::Proxied);
        let r: Resolved =
            serde_json::from_str(r#"{"url":"http://x","delivery":"teleported"}"#).unwrap();
        assert_eq!(r.delivery, Delivery::Direct);
    }

    #[test]
    fn capability_names() {
        let c: Capabilities =
            serde_json::from_str(r#"{"browse":true,"lyrics":true,"radio":true}"#).unwrap();
        assert_eq!(c.names(), ["browse", "lyrics", "radio"]);
        assert!(c.names().iter().all(|n| CAPABILITY_NAMES.contains(n)));
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
