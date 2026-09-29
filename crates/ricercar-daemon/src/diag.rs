//! Pieces of the diagnostic report that do not depend on the interface:
//! system, settings with secrets masked, network state.

use ricercar_core::config::Config;

use crate::{AppContext, NetworkStatus};

const MASK: &str = "(set, hidden)";

/// The settings as TOML, with tokens, keys and sessions hidden.
pub fn masked_config(cfg: &Config) -> String {
    let mut c = cfg.clone();
    for secret in [
        &mut c.scrobble.listenbrainz_token,
        &mut c.scrobble.lastfm_api_key,
        &mut c.scrobble.lastfm_secret,
        &mut c.scrobble.lastfm_session,
    ] {
        if !secret.is_empty() {
            *secret = MASK.into();
        }
    }
    toml::to_string_pretty(&c).unwrap_or_default()
}

/// `PRETTY_NAME` from an os-release file.
/// Mask e-mail addresses (plugins may log the account they signed in
/// with) before log lines go into a report meant to be shared.
pub fn mask_emails(text: &str) -> String {
    let is_local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let is_domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let mut start = out.len();
            for (pos, c) in out.char_indices().rev() {
                if !is_local(c) {
                    break;
                }
                start = pos;
            }
            let mut end = i + 1;
            while end < chars.len() && is_domain(chars[end]) {
                end += 1;
            }
            let domain: String = chars[i + 1..end].iter().collect();
            let domain = domain.trim_end_matches(['.', '-']);
            if start < out.len() && domain.contains('.') {
                out.truncate(start);
                out.push_str("<email>");
                i += 1 + domain.chars().count();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn parse_os_release(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

pub fn distribution() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| parse_os_release(&t))
        .unwrap_or_else(|| "unknown distribution".into())
}

pub fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| format!("Linux {}", s.trim()))
        .unwrap_or_else(|_| "unknown kernel".into())
}

impl AppContext {
    /// One line per declared plugin: state, version, sign-in.
    pub fn plugins_report(&self) -> String {
        use ricercar_core::plugin::RunState;
        let list = self.plugins.statuses();
        if list.is_empty() {
            return "None declared".into();
        }
        list.iter()
            .map(|s| {
                let state = match &s.state {
                    RunState::Disabled => "disabled".to_string(),
                    RunState::Starting => "starting".into(),
                    RunState::Running => "running".into(),
                    RunState::Restarting { in_secs } => format!("restarting in {in_secs} s"),
                    RunState::Failed(m) => format!("failed: {m}"),
                };
                let auth = match &s.auth {
                    Some(a) => format!(" · {:?}", a.state),
                    None => String::new(),
                };
                format!("- {} ({} {}) · {state}{auth}", s.id, s.name, s.version)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Services, port and interfaces, one fact per line.
    pub fn network_report(&self) -> String {
        let cfg = self.config.read().unwrap().network.clone();
        let mut out = vec![format!("Name: {}", cfg.name)];
        out.push(format!(
            "Renderer (UPnP AV + OpenHome): {}",
            if cfg.renderer { "on" } else { "off" }
        ));
        out.push(format!(
            "Media server: {}",
            if cfg.media_server { "on" } else { "off" }
        ));
        out.push(match self.network_status() {
            NetworkStatus::Running { port } => format!("Status: running, HTTP port {port}"),
            NetworkStatus::Starting => "Status: starting".into(),
            NetworkStatus::Off => "Status: off".into(),
            NetworkStatus::Failed(e) => format!("Status: failed ({e})"),
        });
        let ifs = ricercar_upnp::interfaces();
        if ifs.is_empty() {
            out.push("Interfaces: none (no IPv4 address)".into());
        }
        for (name, ip) in ifs {
            out.push(format!("Interface: {name} {ip}"));
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_masked() {
        assert_eq!(
            mask_emails("initialized, account: jane.doe+x@mail.example.org. ok"),
            "initialized, account: <email>. ok"
        );
        assert_eq!(
            mask_emails("user@host and @handle"),
            "user@host and @handle"
        );
        assert_eq!(mask_emails("a@b.c, d@e.fr"), "<email>, <email>");
        assert_eq!(mask_emails("déjà vu"), "déjà vu");
    }

    #[test]
    fn secrets_are_masked() {
        let mut c = Config::default();
        c.scrobble.listenbrainz_token = "0123-secret-token".into();
        c.scrobble.lastfm_api_key = "key-abc".into();
        c.scrobble.lastfm_secret = "shh".into();
        c.scrobble.lastfm_session = "sess-42".into();
        c.scrobble.lastfm_user = "someone".into();
        let t = masked_config(&c);
        for s in ["0123-secret-token", "key-abc", "shh", "sess-42"] {
            assert!(!t.contains(s), "{s} leaked");
        }
        assert!(t.contains("someone"));
        assert!(t.contains(MASK));
        // Empty values stay empty (nothing to hide, and it shows).
        let t = masked_config(&Config::default());
        assert!(!t.contains(MASK));
    }

    #[test]
    fn os_release_pretty_name() {
        let text = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        assert_eq!(parse_os_release(text).as_deref(), Some("Arch Linux"));
        assert_eq!(parse_os_release("ID=x\n"), None);
    }
}
