//! Shared blocking HTTP plumbing: one agent with our User-Agent and timeouts.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::{OnlineError, Result};

/// User-Agent sent with every request. MusicBrainz, LRCLIB and Radio Browser
/// all ask clients to identify themselves with a name, version and contact.
pub const USER_AGENT: &str = "ricercar/0.4 ( https://github.com/ricercar-player/ricercar )";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Error bodies are only kept for diagnostics; cap what we carry around.
const MAX_ERROR_BODY: usize = 512;

pub(crate) fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
    })
}

/// Converts a ureq outcome into our error model (non-2xx becomes `Http`).
pub(crate) fn send(
    outcome: std::result::Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response> {
    match outcome {
        Ok(resp) => Ok(resp),
        Err(ureq::Error::Status(status, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(OnlineError::Http {
                status,
                body: truncate(body, MAX_ERROR_BODY),
            })
        }
        Err(ureq::Error::Transport(t)) => Err(OnlineError::Network(t.to_string())),
    }
}

pub(crate) fn body_string(resp: ureq::Response) -> Result<String> {
    resp.into_string()
        .map_err(|e| OnlineError::Network(format!("reading response body: {e}")))
}

pub(crate) fn parse_json<T: DeserializeOwned>(body: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| OnlineError::Parse(e.to_string()))
}

pub(crate) fn get_json<T: DeserializeOwned>(req: ureq::Request) -> Result<T> {
    let resp = send(req.call())?;
    parse_json(&body_string(resp)?)
}

pub(crate) fn read_bytes(resp: ureq::Response, limit: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|e| OnlineError::Network(format!("reading response body: {e}")))?;
    Ok(buf)
}

pub(crate) fn is_not_found(err: &OnlineError) -> bool {
    matches!(err, OnlineError::Http { status: 404, .. })
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc".into(), 10), "abc");
        assert_eq!(truncate("abcdef".into(), 3), "abc…");
        // 'é' is two bytes; cutting at 1 must back off to 0.
        assert_eq!(truncate("éé".into(), 1), "…");
    }

    #[test]
    fn user_agent_format() {
        assert!(USER_AGENT.starts_with("ricercar/0.4 ("));
    }
}
