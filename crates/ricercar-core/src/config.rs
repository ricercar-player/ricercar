//! User configuration (`$XDG_CONFIG_HOME/ricercar/config.toml`) and XDG paths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(std::env::temp_dir)
        .join("ricercar")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// Logs (`$XDG_STATE_HOME/ricercar`, else `~/.local/state/ricercar`).
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReplayGain {
    /// Bit-perfect: no gain applied.
    #[default]
    Off,
    Track,
    Album,
    /// Album gain when playing an album in order, track gain otherwise.
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub device: String,
    pub replaygain: ReplayGain,
    pub preamp_db: f32,
    /// Resume the previous queue/position at startup (paused).
    pub restore_session: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            device: "default".into(),
            replaygain: ReplayGain::Off,
            preamp_db: 0.0,
            restore_session: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LibraryConfig {
    pub roots: Vec<PathBuf>,
    /// Watch roots for changes (inotify).
    pub watch: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub name: String,
    pub renderer: bool,
    pub media_server: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            name: "ricercar".into(),
            renderer: true,
            media_server: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: Theme,
    /// Accent colour as `#rrggbb`.
    pub accent: String,
    /// Tint the interface with the colours of the current cover.
    pub adaptive_colors: bool,
    pub notifications: bool,
    pub tray: bool,
    /// Keep running in the tray when the window is closed.
    pub close_to_tray: bool,
    /// Interface language: "en", "fr", or empty to follow the system.
    pub language: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            theme: Theme::Dark,
            accent: "#d4a35a".into(),
            adaptive_colors: true,
            notifications: true,
            tray: true,
            close_to_tray: false,
            language: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ScrobbleConfig {
    pub listenbrainz_token: String,
    pub lastfm_api_key: String,
    pub lastfm_secret: String,
    pub lastfm_session: String,
    pub lastfm_user: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnlineConfig {
    /// Fetch synced lyrics from lrclib.net when no local lyrics exist.
    pub lyrics: bool,
    /// Fetch missing album art from MusicBrainz / Cover Art Archive.
    pub cover_art: bool,
    pub radio: bool,
    /// Read the community plugin catalogue (GitHub) on the Plugins page.
    pub plugin_catalog: bool,
    /// Look for a new ricercar release (GitHub) at most once a day, and for
    /// updates of plugins installed from the catalogue.
    pub updates: bool,
}

impl Default for OnlineConfig {
    fn default() -> Self {
        OnlineConfig {
            lyrics: true,
            cover_art: true,
            radio: true,
            plugin_catalog: true,
            updates: true,
        }
    }
}

/// A source plugin: an executable speaking JSON-RPC on stdio
/// (docs/plugins.md). Declared by the user, never downloaded by ricercar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig {
    /// `[a-z0-9-]+`, unique; used in `plugin://<id>/…` URIs.
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Set when installed from the plugin hub (the version installed);
    /// absent for plugins declared by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Host the hub binary was downloaded from, to notice when an update
    /// comes from somewhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `[plugins.settings]`: values of the settings the plugin declares,
    /// only those that differ from their default. Changing them does not
    /// restart the plugin (unless a setting asks for it).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub settings: crate::plugin::settings::Stored,
}

fn yes() -> bool {
    true
}

impl PluginConfig {
    /// Same process: equal but for `settings`, which apply live.
    pub fn same_process(&self, other: &PluginConfig) -> bool {
        self.id == other.id
            && self.command == other.command
            && self.args == other.args
            && self.enabled == other.enabled
            && self.version == other.version
            && self.host == other.host
    }

    pub fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub library: LibraryConfig,
    pub network: NetworkConfig,
    pub ui: UiConfig,
    pub scrobble: ScrobbleConfig,
    pub online: OnlineConfig,
    /// `[[plugins]]` tables.
    pub plugins: Vec<PluginConfig>,
}

impl Config {
    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Missing file → defaults; unreadable file → defaults plus a warning
    /// (never refuse to start because of a typo in the config).
    pub fn load(path: &Path) -> Config {
        let Ok(text) = std::fs::read_to_string(path) else {
            let mut cfg = Config::default();
            cfg.library.watch = true;
            if let Some(music) = default_music_dir() {
                cfg.library.roots.push(music);
            }
            return cfg;
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("config {}: {e}; using defaults", path.display());
            Config::default()
        })
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(path, text.as_bytes())
    }
}

/// `$XDG_MUSIC_DIR` from user-dirs.dirs, else `~/Music` when it exists.
fn default_music_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dirs = std::fs::read_to_string(
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("user-dirs.dirs"),
    )
    .unwrap_or_default();
    let from_dirs = dirs.lines().find_map(|l| {
        let v = l.strip_prefix("XDG_MUSIC_DIR=")?.trim_matches('"');
        Some(PathBuf::from(v.replace("$HOME", &home.to_string_lossy())))
    });
    from_dirs
        .into_iter()
        .chain(std::iter::once(home.join("Music")))
        .find(|p| p.is_dir() && p != &home)
}

/// Write via a temp file + rename so a crash never leaves a truncated file.
/// The file is private to the user (0600: the config holds secrets) and
/// synced to disk before and after the rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private(path, bytes, true)
}

/// Same, without the syncs: for caches that can be rebuilt.
pub(crate) fn write_cache(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private(path, bytes, false)
}

fn write_private(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        if durable {
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    if durable {
        std::fs::File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        let mut c = Config::default();
        c.audio.device = "hw:1,0".into();
        c.library.roots.push("/music".into());
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p), c);

        std::fs::write(&p, "[audio]\nreplaygain = \"album\"\n").unwrap();
        let c = Config::load(&p);
        assert_eq!(c.audio.replaygain, ReplayGain::Album);
        assert_eq!(c.audio.device, "default");
        assert_eq!(c.network.name, "ricercar");
    }

    #[test]
    fn writes_are_private_and_concurrent_safe() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        std::fs::write(&p, b"old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::thread::scope(|s| {
            for i in 0..8 {
                let p = &p;
                s.spawn(move || write_atomic(p, format!("v{i}").as_bytes()).unwrap());
            }
        });
        assert!(std::fs::read_to_string(&p).unwrap().starts_with('v'));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // No temp file left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn text_of(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn plugin_settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "[[plugins]]\nid = \"demo\"\ncommand = \"demo\"\n\n[plugins.settings]\nquality = \"lossless\"\npage_size = 50\nratio = 0.5\nreport_playback = false\n\n[[plugins]]\nid = \"two\"\ncommand = \"two\"\n",
        )
        .unwrap();
        let c = Config::load(&p);
        let s = &c.plugins[0].settings;
        assert_eq!(s["quality"], toml::Value::String("lossless".into()));
        assert_eq!(s["page_size"], toml::Value::Integer(50));
        assert_eq!(s["ratio"], toml::Value::Float(0.5));
        assert_eq!(s["report_playback"], toml::Value::Boolean(false));
        assert!(c.plugins[1].settings.is_empty());
        c.save(&p).unwrap();
        assert!(text_of(&p).contains("[plugins.settings]"));
        assert_eq!(Config::load(&p), c);
        let mut other = c.plugins[0].clone();
        other.settings.clear();
        assert!(other.same_process(&c.plugins[0]) && other != c.plugins[0]);
        other.args.push("x".into());
        assert!(!other.same_process(&c.plugins[0]));
    }

    #[test]
    fn plugin_tables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "[[plugins]]\nid = \"demo\"\ncommand = \"/usr/bin/demo\"\nargs = [\"--serve\"]\n\n[[plugins]]\nid = \"two\"\ncommand = \"two\"\nenabled = false\n",
        )
        .unwrap();
        let c = Config::load(&p);
        assert_eq!(c.plugins.len(), 2);
        assert!(c.plugins[0].enabled && !c.plugins[1].enabled);
        assert_eq!(c.plugins[0].args, ["--serve"]);
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p), c);
        assert!(PluginConfig::valid_id("my-plugin-2"));
        assert!(
            !text_of(&p).contains("settings"),
            "empty settings not written"
        );
        assert!(!PluginConfig::valid_id("My plugin") && !PluginConfig::valid_id(""));
    }
}
