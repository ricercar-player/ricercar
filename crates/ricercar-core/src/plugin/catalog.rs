//! The community plugin hub (github.com/ricercar-player/ricercar-plugins): an index
//! of entries that point at their authors' repositories and release
//! binaries. The hub hosts no code. ricercar reads the index when the user
//! opens the Plugins page, and at startup when a plugin installed from the
//! hub may have an update; it downloads a binary only when the user asks,
//! checks it against the SHA-256 pinned in the index, and runs nothing
//! from the index itself.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::PluginConfig;

pub const INDEX_URL: &str =
    "https://raw.githubusercontent.com/ricercar-player/ricercar-plugins/main/index.json";
/// Largest index accepted.
const MAX_INDEX: u64 = 4 * 1024 * 1024;
/// Largest plugin binary accepted.
const MAX_BINARY: u64 = 256 * 1024 * 1024;

/// A prebuilt binary for one CPU architecture, published by the author.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Asset {
    /// `x86_64` or `aarch64` (as `std::env::consts::ARCH`).
    pub arch: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
}

/// One plugin of the hub.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub repository: String,
    pub homepage: Option<String>,
    pub author: String,
    pub license: String,
    pub version: String,
    pub protocol: u32,
    pub capabilities: Vec<String>,
    /// Arguments to start it with.
    pub args: Vec<String>,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Index {
    #[serde(default)]
    plugins: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    Network(String),
    Invalid(String),
    /// No binary for this machine's architecture.
    NoAsset,
    Checksum {
        expected: String,
        got: String,
    },
    Io(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Network(e) => write!(f, "network error: {e}"),
            CatalogError::Invalid(e) => write!(f, "invalid catalogue: {e}"),
            CatalogError::NoAsset => write!(f, "no binary for this computer"),
            CatalogError::Checksum { expected, got } => {
                write!(f, "checksum mismatch (expected {expected}, got {got})")
            }
            CatalogError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Host part of an `https://` or `file://` URL, lowercased.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |a| a.1);
    Some(host.to_ascii_lowercase())
}

fn https(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

pub fn current_arch() -> &'static str {
    std::env::consts::ARCH
}

impl Entry {
    /// Entries that fail these checks are left out of the catalogue.
    /// `local` also accepts `file://` binaries (a local index).
    fn valid(&self, local: bool) -> bool {
        PluginConfig::valid_id(&self.id)
            && !self.name.trim().is_empty()
            && https(&self.repository)
            && self.assets.iter().all(|a| {
                (https(&a.url) || (local && a.url.starts_with("file://")))
                    && a.sha256.len() == 64
                    && a.sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
    }

    pub fn asset_for(&self, arch: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.arch == arch)
    }

    /// Can be installed here: same protocol and a binary for this machine.
    pub fn installable(&self) -> bool {
        self.protocol == super::PROTOCOL && self.asset_for(current_arch()).is_some()
    }
}

/// Parse an index; invalid entries are dropped, and a duplicate id keeps
/// its first entry. `local`: an index from this computer (tests, tours).
pub fn parse_index(json: &str, local: bool) -> Result<Vec<Entry>, CatalogError> {
    let idx: Index =
        serde_json::from_str(json).map_err(|e| CatalogError::Invalid(e.to_string()))?;
    let mut seen = std::collections::HashSet::new();
    Ok(idx
        .plugins
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Entry>(v).ok())
        .filter(|e| e.valid(local) && seen.insert(e.id.clone()))
        .collect())
}

fn read_limited(r: impl Read, max: u64) -> Result<Vec<u8>, CatalogError> {
    let mut buf = Vec::new();
    r.take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| CatalogError::Network(e.to_string()))?;
    if buf.len() as u64 > max {
        return Err(CatalogError::Invalid("file too large".into()));
    }
    Ok(buf)
}

/// Bytes of an `https://` URL, or of a local `file://` URL / path when
/// `local` (tests and the headless tours only).
fn fetch(url: &str, max: u64, local: bool) -> Result<Vec<u8>, CatalogError> {
    if local
        && let Some(p) = url
            .strip_prefix("file://")
            .or(url.starts_with('/').then_some(url))
    {
        let f = std::fs::File::open(crate::meta::percent_decode(p))
            .map_err(|e| CatalogError::Io(e.to_string()))?;
        return read_limited(f, max);
    }
    if !https(url) {
        return Err(CatalogError::Invalid(format!("not an https URL: {url}")));
    }
    let resp = ureq::get(url)
        .set(
            "User-Agent",
            concat!("ricercar/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(Duration::from_secs(60))
        .call()
        .map_err(|e| CatalogError::Network(e.to_string()))?;
    read_limited(resp.into_reader(), max)
}

/// The hub's index. `url` may be a local file when `local` is set.
pub fn fetch_index(url: &str, local: bool) -> Result<Vec<Entry>, CatalogError> {
    let bytes = fetch(url, MAX_INDEX, local)?;
    parse_index(&String::from_utf8_lossy(&bytes), local)
}

/// Where hub plugins are installed: `<data_dir>/plugin-bin/<id>/<version>/`
/// (next to, not inside, the plugin's own data directory).
pub fn bin_root(data_dir: &Path) -> PathBuf {
    data_dir.join("plugin-bin")
}

/// Download the binary for this machine, check its SHA-256, install it and
/// return the declaration to add to the config. Older versions of the same
/// plugin are removed once the new one is in place.
pub fn install(entry: &Entry, root: &Path, local: bool) -> Result<PluginConfig, CatalogError> {
    let asset = entry
        .asset_for(current_arch())
        .ok_or(CatalogError::NoAsset)?;
    let bytes = fetch(&asset.url, MAX_BINARY, local)?;
    let got = sha256_hex(&bytes);
    if got != asset.sha256 {
        return Err(CatalogError::Checksum {
            expected: asset.sha256.clone(),
            got,
        });
    }
    let version = if entry.version.is_empty() {
        "0".to_string()
    } else {
        entry
            .version
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || ".-_+".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let dir = root.join(&entry.id).join(&version);
    let io = |e: std::io::Error| CatalogError::Io(e.to_string());
    std::fs::create_dir_all(&dir).map_err(io)?;
    let path = dir.join(&entry.id);
    let tmp = dir.join(format!(".{}.part", entry.id));
    std::fs::write(&tmp, &bytes).map_err(io)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
    }
    std::fs::rename(&tmp, &path).map_err(io)?;
    if let Ok(dirs) = std::fs::read_dir(root.join(&entry.id)) {
        for d in dirs.flatten() {
            if d.file_name() != version.as_str() {
                let _ = std::fs::remove_dir_all(d.path());
            }
        }
    }
    Ok(PluginConfig {
        id: entry.id.clone(),
        command: path,
        args: entry.args.clone(),
        enabled: true,
        version: Some(entry.version.clone()),
        host: url_host(&asset.url),
        settings: Default::default(),
    })
}

/// Remove an installed plugin's binaries (its data directory is left:
/// the plugin owns it, and it may hold the user's sign-in).
pub fn uninstall(root: &Path, id: &str) -> Result<(), CatalogError> {
    if !PluginConfig::valid_id(id) {
        return Err(CatalogError::Invalid(format!("bad id {id}")));
    }
    match std::fs::remove_dir_all(root.join(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CatalogError::Io(e.to_string())),
    }
}

/// A newer catalogue version of an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    /// The binary for this machine is now served from another host than
    /// the installed one: ask the user again before installing.
    pub host_changed: bool,
}

/// The update offered for an installed plugin, if the catalogue version is
/// strictly newer (a pre-release is older than its release).
pub fn update_info(installed: &PluginConfig, entry: &Entry) -> Option<UpdateInfo> {
    let current = installed.version.as_deref()?;
    let newer = !entry.version.trim().is_empty()
        && entry
            .version
            .trim_start_matches('v')
            .starts_with(|c: char| c.is_ascii_digit())
        && crate::update::compare(&entry.version, current) == std::cmp::Ordering::Greater;
    if !newer {
        return None;
    }
    let now = entry
        .asset_for(current_arch())
        .and_then(|a| url_host(&a.url));
    Some(UpdateInfo {
        version: entry.version.clone(),
        host_changed: installed.host.is_some() && now != installed.host,
    })
}

/// Whether a newer catalogue version exists for an installed plugin.
pub fn update_available(installed: &PluginConfig, entry: &Entry) -> bool {
    update_info(installed, entry).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, sha: &str) -> Entry {
        Entry {
            id: "demo".into(),
            name: "Demo".into(),
            repository: "https://example.org/demo".into(),
            version: "1.0.0".into(),
            protocol: 1,
            args: vec!["--serve".into()],
            assets: vec![Asset {
                arch: current_arch().into(),
                url: url.into(),
                sha256: sha.into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn index_validation() {
        let good = "0".repeat(64);
        let json = serde_json::json!({"version": 1, "plugins": [
            {"id": "ok", "name": "OK", "repository": "https://x/ok", "protocol": 1,
             "assets": [{"arch": "x86_64", "url": "https://x/ok.bin", "sha256": good}]},
            {"id": "manual", "name": "Manual", "repository": "https://x/m", "protocol": 1},
            {"id": "ok", "name": "Duplicate", "repository": "https://x/d"},
            {"id": "Bad Id", "name": "B", "repository": "https://x/b"},
            {"id": "plain", "name": "P", "repository": "http://x/p"},
            {"id": "sha", "name": "S", "repository": "https://x/s",
             "assets": [{"arch": "x86_64", "url": "https://x/s", "sha256": "ABC"}]},
            {"id": "extra", "name": "E", "repository": "https://x/e", "unknown": true},
            "not an object"
        ]});
        let list = parse_index(&json.to_string(), false).unwrap();
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["ok", "manual", "extra"]);
        assert_eq!(list[0].name, "OK");
        assert!(list[0].asset_for("x86_64").is_some() && list[0].asset_for("riscv64").is_none());
        assert!(!list[1].installable());
        assert!(parse_index("{", false).is_err());
        let local =
            serde_json::json!({"plugins": [{"id": "l", "name": "L", "repository": "https://x/l",
            "assets": [{"arch": "x86_64", "url": "file:///tmp/l", "sha256": "0".repeat(64)}]}]})
            .to_string();
        assert!(parse_index(&local, false).unwrap().is_empty());
        assert_eq!(parse_index(&local, true).unwrap().len(), 1);
    }

    #[test]
    fn install_checks_the_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("plugin-binary");
        std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
        let sha = sha256_hex(b"#!/bin/sh\necho hi\n");
        let url = format!("file://{}", bin.display());
        let root = bin_root(dir.path());

        // Local files only with `local`.
        assert!(matches!(
            install(&entry(&url, &sha), &root, false),
            Err(CatalogError::Invalid(_))
        ));
        let bad = install(&entry(&url, &"0".repeat(64)), &root, true);
        assert!(matches!(bad, Err(CatalogError::Checksum { .. })));
        assert!(
            !root.join("demo").exists()
                || std::fs::read_dir(root.join("demo")).unwrap().all(|d| {
                    std::fs::read_dir(d.unwrap().path())
                        .unwrap()
                        .next()
                        .is_none()
                })
        );

        let cfg = install(&entry(&url, &sha), &root, true).unwrap();
        assert_eq!(cfg.command, root.join("demo/1.0.0/demo"));
        assert_eq!(
            (cfg.args.as_slice(), cfg.version.as_deref()),
            (&["--serve".to_string()][..], Some("1.0.0"))
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&cfg.command)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );

        // An update replaces the old version.
        let mut e2 = entry(&url, &sha);
        e2.version = "1.1.0".into();
        assert!(update_available(&cfg, &e2));
        let cfg2 = install(&e2, &root, true).unwrap();
        assert!(cfg2.command.exists() && !cfg.command.exists());
        assert_eq!(cfg2.host.as_deref(), Some(""));

        uninstall(&root, "demo").unwrap();
        assert!(!root.join("demo").exists());
        uninstall(&root, "demo").unwrap();
        assert!(uninstall(&root, "../x").is_err());
    }

    #[test]
    fn updates_only_go_forward() {
        let sha = "0".repeat(64);
        let mut e = entry("https://github.com/a/b/releases/download/v1/p", &sha);
        let installed = |v: &str| PluginConfig {
            id: "demo".into(),
            command: "/x".into(),
            args: vec![],
            enabled: true,
            version: Some(v.into()),
            host: Some("github.com".into()),
            settings: Default::default(),
        };
        e.version = "1.0.0".into();
        assert!(update_info(&installed("1.0.0"), &e).is_none());
        assert!(update_info(&installed("1.1.0"), &e).is_none());
        assert!(update_info(&installed("1.0.0-rc.1"), &e).is_some());
        e.version = "1.0.1-beta".into();
        assert!(update_info(&installed("1.0.1"), &e).is_none());
        e.version = "nightly".into();
        assert!(update_info(&installed("1.0.0"), &e).is_none());
        e.version = "1.2.0".into();
        assert_eq!(
            update_info(&installed("1.0.0"), &e),
            Some(UpdateInfo {
                version: "1.2.0".into(),
                host_changed: false
            })
        );
        e.assets[0].url = "https://evil.example/p".into();
        assert!(update_info(&installed("1.0.0"), &e).unwrap().host_changed);
        let mut by_hand = installed("1.0.0");
        by_hand.version = None;
        assert!(!update_available(&by_hand, &e));
        assert_eq!(
            url_host("https://User@Example.org:443/x?y").as_deref(),
            Some("example.org:443")
        );
    }

    #[test]
    fn digest_is_standard() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
