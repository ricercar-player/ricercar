//! Update check: at most once a day, ask GitHub for ricercar's latest
//! release (no account, `ETag` so an unchanged answer costs nothing) and
//! compare it with the running version. Nothing is downloaded or installed.

use std::cmp::Ordering;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const LATEST_URL: &str =
    "https://api.github.com/repos/ricercar-player/ricercar/releases/latest";
/// Seconds between two checks.
pub const INTERVAL: i64 = 24 * 3600;

/// A published release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    /// The release page.
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
    let mut req = ureq::get(LATEST_URL)
        .set(
            "User-Agent",
            concat!("ricercar/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20));
    if let Some(tag) = &state.etag {
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

/// `tag_name` and `html_url` of GitHub's release JSON.
pub fn parse_release(json: &str) -> Result<Release, String> {
    #[derive(Deserialize)]
    struct Gh {
        tag_name: String,
        html_url: String,
    }
    let gh: Gh = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if !gh.html_url.starts_with("https://github.com/") {
        return Err(format!("unexpected release address: {}", gh.html_url));
    }
    Ok(Release {
        version: gh.tag_name.trim_start_matches('v').to_string(),
        url: gh.html_url,
    })
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
