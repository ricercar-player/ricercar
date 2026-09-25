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
}

impl Default for OnlineConfig {
    fn default() -> Self {
        OnlineConfig {
            lyrics: true,
            cover_art: true,
            radio: true,
        }
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
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
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
}
