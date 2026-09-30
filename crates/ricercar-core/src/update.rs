//! Update check: at most once a day, ask GitHub for ricercar's latest
//! release (no account, `ETag` so an unchanged answer costs nothing) and
//! compare it with the running version. Installing is a separate, explicit
//! step ([`install`]): the release file matching how ricercar was installed
//! is downloaded, checked against the release's `SHA256SUMS`, then either
//! replaces the AppImage or goes to the package manager through `pkexec`.

use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const LATEST_URL: &str =
    "https://api.github.com/repos/ricercar-player/ricercar/releases/latest";
/// Seconds between two checks.
pub const INTERVAL: i64 = 24 * 3600;
/// Release files are only taken from here.
const DOWNLOADS: &str = "https://github.com/ricercar-player/ricercar/releases/download/";
const SUMS: &str = "SHA256SUMS";
const MAX_FILE: u64 = 512 << 20;

/// A published release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    /// The release page.
    pub url: String,
    /// Its files (empty in states saved before 0.5.2).
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A file attached to a release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// What the last check learnt, kept between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// Unix time of the last check.
    pub checked: i64,
    pub etag: Option<String>,
    pub latest: Option<Release>,
    /// Version whose notice the user closed: not shown again.
    pub dismissed: Option<String>,
}

impl UpdateState {
    pub fn load(path: &Path) -> UpdateState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::config::write_atomic(path, text.as_bytes())
    }

    /// The known release when it is newer than `current` and its notice
    /// was not closed.
    pub fn newer_than(&self, current: &str) -> Option<&Release> {
        self.latest.as_ref().filter(|r| {
            compare(&r.version, current) == Ordering::Greater
                && self.dismissed.as_deref() != Some(r.version.as_str())
        })
    }
}

/// Refresh `state` unless it was checked less than a day before `now`.
/// Network errors leave it as it was.
pub fn check(state: &mut UpdateState, now: i64) -> Result<(), String> {
    if now - state.checked < INTERVAL && now >= state.checked {
        return Ok(());
    }
    refresh(state, now)
}

/// Ask GitHub now, whenever the last check was ("Check for updates").
pub fn refresh(state: &mut UpdateState, now: i64) -> Result<(), String> {
    let mut req = ureq::get(LATEST_URL)
        .set(
            "User-Agent",
            concat!("ricercar/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20));
    // A state saved without the release files needs a full answer.
    let has_assets = state.latest.as_ref().is_some_and(|r| !r.assets.is_empty());
    if let Some(tag) = state.etag.as_ref().filter(|_| has_assets) {
        req = req.set("If-None-Match", tag);
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    state.checked = now;
    if resp.status() == 304 {
        return Ok(());
    }
    let etag = resp.header("ETag").map(str::to_string);
    let body = resp.into_string().map_err(|e| e.to_string())?;
    state.latest = Some(parse_release(&body)?);
    state.etag = etag;
    Ok(())
}

/// `tag_name`, `html_url` and the files of GitHub's release JSON; files
/// served from anywhere but ricercar's releases are left out.
pub fn parse_release(json: &str) -> Result<Release, String> {
    #[derive(Deserialize)]
    struct Gh {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        assets: Vec<GhAsset>,
    }
    #[derive(Deserialize)]
    struct GhAsset {
        name: String,
        browser_download_url: String,
    }
    let gh: Gh = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if !gh.html_url.starts_with("https://github.com/") {
        return Err(format!("unexpected release address: {}", gh.html_url));
    }
    Ok(Release {
        version: gh.tag_name.trim_start_matches('v').to_string(),
        url: gh.html_url,
        assets: gh
            .assets
            .into_iter()
            .filter(|a| a.browser_download_url.starts_with(DOWNLOADS))
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// How the running ricercar was installed, which decides how it updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// A portable AppImage, replaced in place.
    AppImage(PathBuf),
    /// A package of the release, upgraded by the package manager.
    Package(Manager, PathBuf),
    /// Built from source, unpacked by hand, or a distribution's own
    /// package: the release page is the only offer.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Pacman,
    Deb,
    Rpm,
}

impl Install {
    /// The program to start again once the update is in place.
    pub fn program(&self) -> Option<&Path> {
        match self {
            Install::AppImage(p) | Install::Package(_, p) => Some(p),
            Install::Manual => None,
        }
    }
}

/// Found once per run: asks the package managers who owns the binary.
pub fn install_kind() -> &'static Install {
    static KIND: OnceLock<Install> = OnceLock::new();
    KIND.get_or_init(detect)
}

fn detect() -> Install {
    if let Some(p) = std::env::var_os("APPIMAGE").map(PathBuf::from)
        && p.is_file()
    {
        return Install::AppImage(p);
    }
    let Ok(exe) = std::env::current_exe() else {
        return Install::Manual;
    };
    if !exe.starts_with("/usr/") {
        return Install::Manual;
    }
    let owner = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).arg(&exe).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Only the release's own package, named `ricercar`, is upgraded with a
    // release file (not `ricercar-git` or a distribution's build).
    if owner("pacman", &["-Qqo"]).as_deref() == Some("ricercar") {
        return Install::Package(Manager::Pacman, exe);
    }
    if owner("dpkg-query", &["-S"]).is_some_and(|o| o.starts_with("ricercar:")) {
        return Install::Package(Manager::Deb, exe);
    }
    if owner("rpm", &["-qf", "--qf", "%{NAME}"]).as_deref() == Some("ricercar") {
        return Install::Package(Manager::Rpm, exe);
    }
    Install::Manual
}

/// The release file for this kind of install and machine, named as the
/// release workflow names them.
pub fn asset_for<'a>(release: &'a Release, kind: &Install, arch: &str) -> Option<&'a Asset> {
    let v = &release.version;
    let matches = |name: &str| match kind {
        Install::AppImage(_) => name == format!("ricercar-{v}-{arch}.AppImage"),
        Install::Package(Manager::Pacman, _) => {
            name.starts_with(&format!("ricercar-{v}-"))
                && name.ends_with(&format!("-{arch}.pkg.tar.zst"))
        }
        Install::Package(Manager::Deb, _) => {
            let deb = match arch {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                a => a,
            };
            name.starts_with(&format!("ricercar_{v}-")) && name.ends_with(&format!("_{deb}.deb"))
        }
        Install::Package(Manager::Rpm, _) => {
            name.starts_with(&format!("ricercar-{v}-")) && name.ends_with(&format!(".{arch}.rpm"))
        }
        Install::Manual => false,
    };
    release.assets.iter().find(|a| matches(&a.name))
}

/// Can `release` be installed from ricercar itself?
pub fn installable(release: &Release, kind: &Install) -> bool {
    asset_for(release, kind, std::env::consts::ARCH).is_some()
        && release.assets.iter().any(|a| a.name == SUMS)
}

/// `sha256sum` output: the digest of `name`, if listed.
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with(DOWNLOADS) {
        return Err(format!("unexpected download address: {url}"));
    }
    let resp = ureq::get(url)
        .set(
            "User-Agent",
            concat!("ricercar/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_FILE + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_FILE {
        return Err("file too large".into());
    }
    Ok(buf)
}

/// Why an update did not install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// Nothing to install for this kind of install or machine.
    NoFile,
    Network(String),
    /// The file does not match `SHA256SUMS`.
    Checksum,
    /// The password prompt was closed.
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::NoFile => write!(f, "no release file for this system"),
            InstallError::Network(e) => write!(f, "download: {e}"),
            InstallError::Checksum => write!(f, "the download does not match SHA256SUMS"),
            InstallError::Cancelled => write!(f, "cancelled"),
            InstallError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Download the release file for `kind`, check it, and put it in place.
/// `work` holds downloaded packages until their manager has read them.
pub fn install(release: &Release, kind: &Install, work: &Path) -> Result<(), InstallError> {
    let arch = std::env::consts::ARCH;
    let asset = asset_for(release, kind, arch).ok_or(InstallError::NoFile)?;
    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == SUMS)
        .ok_or(InstallError::NoFile)?;
    let sums = download(&sums_asset.url).map_err(InstallError::Network)?;
    let want =
        sum_for(&String::from_utf8_lossy(&sums), &asset.name).ok_or(InstallError::Checksum)?;
    let bytes = download(&asset.url).map_err(InstallError::Network)?;
    if crate::plugin::catalog::sha256_hex(&bytes) != want {
        return Err(InstallError::Checksum);
    }
    let io = |e: std::io::Error| InstallError::Failed(e.to_string());
    match kind {
        Install::AppImage(target) => replace_file(target, &bytes).map_err(io),
        Install::Package(manager, _) => {
            std::fs::create_dir_all(work).map_err(io)?;
            let file = work.join(&asset.name);
            std::fs::write(&file, &bytes).map_err(io)?;
            let result = run_manager(*manager, &file);
            let _ = std::fs::remove_file(&file);
            result
        }
        Install::Manual => Err(InstallError::NoFile),
    }
}

/// Write next to `target`, then rename over it: a running AppImage keeps
/// its old file until it exits.
fn replace_file(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = target.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(".ricercar-update.part");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
}

/// The package manager's command for a local package file.
fn manager_command(manager: Manager, file: &Path, has: impl Fn(&str) -> bool) -> Vec<String> {
    let f = file.to_string_lossy().into_owned();
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).chain([f.clone()]).collect();
    match manager {
        Manager::Pacman => v(&["pacman", "-U", "--noconfirm"]),
        Manager::Deb if has("apt-get") => v(&["apt-get", "install", "-y"]),
        Manager::Deb => v(&["dpkg", "-i"]),
        Manager::Rpm if has("dnf") => v(&["dnf", "install", "-y"]),
        Manager::Rpm if has("zypper") => v(&[
            "zypper",
            "--non-interactive",
            "install",
            "--allow-unsigned-rpm",
        ]),
        Manager::Rpm => v(&["rpm", "-U"]),
    }
}

/// Run the package manager as root; `pkexec` shows the desktop's password
/// prompt.
fn run_manager(manager: Manager, file: &Path) -> Result<(), InstallError> {
    if !has("pkexec") {
        return Err(InstallError::Failed("pkexec is not installed".into()));
    }
    let cmd = manager_command(manager, file, has);
    let out = Command::new("pkexec")
        .args(&cmd)
        .output()
        .map_err(|e| InstallError::Failed(e.to_string()))?;
    match out.status.code() {
        Some(0) => Ok(()),
        // pkexec: the dialog was dismissed, or authorisation refused.
        Some(126 | 127) => Err(InstallError::Cancelled),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            Err(InstallError::Failed(format!("{}: {}", cmd[0], line.trim())))
        }
    }
}

/// Semantic-version order: `1.2.3` > `1.2.3-rc.1` > `1.2.3-alpha`; a
/// leading `v` and build metadata (`+…`) are ignored. Unreadable parts
/// count as 0.
pub fn compare(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.trim().trim_start_matches('v');
        let v = v.split('+').next().unwrap_or("");
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let mut nums: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        nums.resize(3, 0);
        (nums, pre)
    }
    fn pre_cmp(a: &str, b: &str) -> Ordering {
        let (mut x, mut y) = (a.split('.'), b.split('.'));
        loop {
            return match (x.next(), y.next()) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(p), Some(q)) => {
                    let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                        (Ok(m), Ok(n)) => m.cmp(&n),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => p.cmp(q),
                    };
                    if o == Ordering::Equal {
                        continue;
                    }
                    o
                }
            };
        }
    }
    let ((na, pa), (nb, pb)) = (split(a), split(b));
    na.cmp(&nb).then_with(|| match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(p), Some(q)) => pre_cmp(p, q),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_like_semver() {
        use Ordering::*;
        assert_eq!(compare("0.4.1", "0.4.0"), Greater);
        assert_eq!(compare("v0.10.0", "0.9.9"), Greater);
        assert_eq!(compare("0.4.0", "v0.4.0"), Equal);
        assert_eq!(compare("0.4", "0.4.0"), Equal);
        assert_eq!(compare("0.5.0-alpha", "0.5.0"), Less);
        assert_eq!(compare("0.5.0", "0.5.0-rc.1"), Greater);
        assert_eq!(compare("0.5.0-alpha.2", "0.5.0-alpha.10"), Less);
        assert_eq!(compare("0.5.0-alpha", "0.5.0-beta"), Less);
        assert_eq!(compare("0.5.0-alpha", "0.5.0-alpha.1"), Less);
        assert_eq!(compare("0.5.0-1", "0.5.0-alpha"), Less);
        assert_eq!(compare("0.5.0-alpha", "0.4.9"), Greater);
        assert_eq!(compare("1.0.0+build.5", "1.0.0"), Equal);
    }

    #[test]
    fn release_json_and_state() {
        let r = parse_release(
            r#"{"tag_name":"v0.5.0","html_url":"https://github.com/ricercar-player/ricercar/releases/tag/v0.5.0","body":"x"}"#,
        )
        .unwrap();
        assert_eq!(r.version, "0.5.0");
        assert!(parse_release(r#"{"tag_name":"v1","html_url":"https://evil.example/"}"#).is_err());
        let st = UpdateState {
            latest: Some(r),
            ..Default::default()
        };
        assert!(st.newer_than("0.4.0").is_some());
        assert!(st.newer_than("0.5.0").is_none());
        assert!(st.newer_than("0.6.0-alpha").is_none());
        let closed = UpdateState {
            dismissed: Some("0.5.0".into()),
            ..st.clone()
        };
        assert!(closed.newer_than("0.4.0").is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/update.json");
        st.save(&path).unwrap();
        assert_eq!(UpdateState::load(&path).latest, st.latest);
        assert!(UpdateState::load(&dir.path().join("none")).latest.is_none());
    }

    fn release() -> Release {
        let names = [
            "SHA256SUMS",
            "ricercar-0.6.0-1-x86_64.pkg.tar.zst",
            "ricercar-0.6.0-1.aarch64.rpm",
            "ricercar-0.6.0-1.x86_64.rpm",
            "ricercar-0.6.0-aarch64.AppImage",
            "ricercar-0.6.0-linux-x86_64.tar.gz",
            "ricercar-0.6.0-x86_64.AppImage",
            "ricercar_0.6.0-1_amd64.deb",
            "ricercar_0.6.0-1_arm64.deb",
        ];
        Release {
            version: "0.6.0".into(),
            url: "https://github.com/ricercar-player/ricercar/releases/tag/v0.6.0".into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: format!("{DOWNLOADS}v0.6.0/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn release_files() {
        let json = r#"{"tag_name":"v0.6.0","html_url":"https://github.com/ricercar-player/ricercar/releases/tag/v0.6.0",
            "assets":[{"name":"SHA256SUMS","browser_download_url":"https://github.com/ricercar-player/ricercar/releases/download/v0.6.0/SHA256SUMS"},
                      {"name":"x.AppImage","browser_download_url":"https://evil.example/x.AppImage"}]}"#;
        let r = parse_release(json).unwrap();
        assert_eq!(r.assets.len(), 1);
        assert_eq!(r.assets[0].name, "SHA256SUMS");
        // A state saved by 0.5.1 has no files.
        let old: UpdateState =
            serde_json::from_str(r#"{"checked":1,"latest":{"version":"0.5.0","url":"u"}}"#)
                .unwrap();
        assert!(old.latest.unwrap().assets.is_empty());
    }

    #[test]
    fn file_for_each_kind_of_install() {
        let r = release();
        let p = PathBuf::from("/x");
        let name = |k: Install, arch: &str| asset_for(&r, &k, arch).map(|a| a.name.clone());
        assert_eq!(
            name(Install::AppImage(p.clone()), "x86_64").as_deref(),
            Some("ricercar-0.6.0-x86_64.AppImage")
        );
        assert_eq!(
            name(Install::AppImage(p.clone()), "aarch64").as_deref(),
            Some("ricercar-0.6.0-aarch64.AppImage")
        );
        assert_eq!(
            name(Install::Package(Manager::Pacman, p.clone()), "x86_64").as_deref(),
            Some("ricercar-0.6.0-1-x86_64.pkg.tar.zst")
        );
        assert_eq!(
            name(Install::Package(Manager::Pacman, p.clone()), "aarch64"),
            None
        );
        assert_eq!(
            name(Install::Package(Manager::Deb, p.clone()), "aarch64").as_deref(),
            Some("ricercar_0.6.0-1_arm64.deb")
        );
        assert_eq!(
            name(Install::Package(Manager::Rpm, p.clone()), "x86_64").as_deref(),
            Some("ricercar-0.6.0-1.x86_64.rpm")
        );
        assert_eq!(name(Install::Manual, "x86_64"), None);
        assert!(installable(&r, &Install::Package(Manager::Deb, p.clone())));
        let mut no_sums = r.clone();
        no_sums.assets.retain(|a| a.name != "SHA256SUMS");
        assert!(!installable(&no_sums, &Install::Package(Manager::Deb, p)));
    }

    #[test]
    fn checksum_lines() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  ricercar-0.6.0-x86_64.AppImage\n{b} *ricercar_0.6.0-1_amd64.deb\nbad  x\n"
        );
        assert_eq!(sum_for(&sums, "ricercar-0.6.0-x86_64.AppImage"), Some(a));
        assert_eq!(
            sum_for(&sums, "ricercar_0.6.0-1_amd64.deb"),
            Some("b".repeat(64))
        );
        assert_eq!(sum_for(&sums, "x"), None);
        assert_eq!(sum_for(&sums, "missing"), None);
    }

    #[test]
    fn package_manager_commands() {
        let f = Path::new("/c/p.rpm");
        let none = |_: &str| false;
        assert_eq!(
            manager_command(Manager::Pacman, f, none),
            ["pacman", "-U", "--noconfirm", "/c/p.rpm"]
        );
        assert_eq!(
            manager_command(Manager::Deb, f, |c| c == "apt-get"),
            ["apt-get", "install", "-y", "/c/p.rpm"]
        );
        assert_eq!(
            manager_command(Manager::Deb, f, none),
            ["dpkg", "-i", "/c/p.rpm"]
        );
        assert_eq!(
            manager_command(Manager::Rpm, f, |c| c == "zypper")[0..2],
            ["zypper", "--non-interactive"]
        );
        assert_eq!(
            manager_command(Manager::Rpm, f, none),
            ["rpm", "-U", "/c/p.rpm"]
        );
    }

    #[test]
    fn appimage_is_replaced_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ricercar.AppImage");
        std::fs::write(&target, b"old").unwrap();
        replace_file(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn recent_check_is_not_repeated() {
        // Checked an hour ago: no network call, state untouched.
        let mut st = UpdateState {
            checked: 10_000,
            ..Default::default()
        };
        assert!(check(&mut st, 10_000 + 3600).is_ok());
        assert_eq!(st.checked, 10_000);
    }
}
