//! ListenBrainz client (<https://listenbrainz.readthedocs.io/en/latest/users/api/core.html>).
//! Authenticates with the user's personal token from their profile page.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::http::{self, agent};
use crate::scrobble::{Scrobble, ScrobbleTrack};
use crate::{OnlineError, Result};

pub const API_BASE: &str = "https://api.listenbrainz.org";
/// Listens sent per request; the server accepts up to 1000 but also caps the
/// payload size, so stay well below.
pub const MAX_BATCH: usize = 100;

#[derive(Debug, Clone)]
pub struct ListenBrainz {
    token: String,
    base: String,
}

#[derive(Deserialize)]
struct ValidateResponse {
    #[serde(default)]
    valid: bool,
    user_name: Option<String>,
}

impl ListenBrainz {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into().trim().to_owned(),
            base: API_BASE.to_owned(),
        }
    }

    /// Targets another ListenBrainz-compatible server (e.g. self-hosted).
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_owned();
        self
    }

    /// Returns the user name the token belongs to, or `None` if invalid.
    pub fn validate_token(&self) -> Result<Option<String>> {
        let req = agent()
            .get(&format!("{}/1/validate-token", self.base))
            .set("Authorization", &self.auth_header()?);
        match http::get_json::<ValidateResponse>(req) {
            Ok(r) => Ok(parse_validate(r)),
            Err(OnlineError::Http {
                status: 400 | 401, ..
            }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn playing_now(&self, track: &ScrobbleTrack) -> Result<()> {
        self.post(&listen_payload("playing_now", &[(track, None)]))
    }

    /// Submits listens: `single` for one, `import` batches for several.
    pub fn submit(&self, scrobbles: &[Scrobble]) -> Result<()> {
        for chunk in scrobbles.chunks(MAX_BATCH) {
            let kind = if chunk.len() == 1 { "single" } else { "import" };
            let items: Vec<_> = chunk
                .iter()
                .map(|s| (&s.track, Some(s.started_at_unix)))
                .collect();
            self.post(&listen_payload(kind, &items))?;
        }
        Ok(())
    }

    fn auth_header(&self) -> Result<String> {
        if self.token.is_empty() {
            return Err(OnlineError::NotConfigured);
        }
        Ok(format!("Token {}", self.token))
    }

    fn post(&self, body: &Value) -> Result<()> {
        let req = agent()
            .post(&format!("{}/1/submit-listens", self.base))
            .set("Authorization", &self.auth_header()?)
            .set("Content-Type", "application/json");
        match http::send(req.send_string(&body.to_string())) {
            Ok(_) => Ok(()),
            Err(OnlineError::Http { status: 401, body }) => Err(OnlineError::Auth(body)),
            Err(e) => Err(e),
        }
    }
}

fn parse_validate(r: ValidateResponse) -> Option<String> {
    r.valid.then_some(r.user_name).flatten()
}

/// Builds a `submit-listens` body; `listened_at` is omitted for `playing_now`.
pub(crate) fn listen_payload(listen_type: &str, items: &[(&ScrobbleTrack, Option<i64>)]) -> Value {
    let payload: Vec<Value> = items
        .iter()
        .map(|(track, listened_at)| {
            let mut listen = Map::new();
            if let Some(ts) = listened_at {
                listen.insert("listened_at".into(), json!(ts));
            }
            listen.insert("track_metadata".into(), track_metadata(track));
            Value::Object(listen)
        })
        .collect();
    json!({ "listen_type": listen_type, "payload": payload })
}

fn track_metadata(t: &ScrobbleTrack) -> Value {
    let mut info = Map::new();
    info.insert("media_player".into(), json!("ricercar"));
    info.insert("submission_client".into(), json!("ricercar"));
    info.insert(
        "submission_client_version".into(),
        json!(env!("CARGO_PKG_VERSION")),
    );
    if t.duration_ms > 0 {
        info.insert("duration_ms".into(), json!(t.duration_ms));
    }
    if let Some(n) = t.track_number {
        info.insert("tracknumber".into(), json!(n));
    }
    if let Some(mbid) = non_empty(&t.mbid) {
        info.insert("recording_mbid".into(), json!(mbid));
    }
    let mut meta = Map::new();
    meta.insert("artist_name".into(), json!(t.artist));
    meta.insert("track_name".into(), json!(t.title));
    if let Some(album) = non_empty(&t.album) {
        meta.insert("release_name".into(), json!(album));
    }
    meta.insert("additional_info".into(), Value::Object(info));
    Value::Object(meta)
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> ScrobbleTrack {
        ScrobbleTrack {
            artist: "Radiohead".into(),
            title: "Airbag".into(),
            album: Some("OK Computer".into()),
            album_artist: Some("Radiohead".into()),
            duration_ms: 284_000,
            track_number: Some(1),
            mbid: Some("1a2b".into()),
        }
    }

    #[test]
    fn single_listen_payload() {
        let t = track();
        let v = listen_payload("single", &[(&t, Some(1_700_000_000))]);
        let expected = json!({
            "listen_type": "single",
            "payload": [{
                "listened_at": 1_700_000_000,
                "track_metadata": {
                    "artist_name": "Radiohead",
                    "track_name": "Airbag",
                    "release_name": "OK Computer",
                    "additional_info": {
                        "media_player": "ricercar",
                        "submission_client": "ricercar",
                        "submission_client_version": env!("CARGO_PKG_VERSION"),
                        "duration_ms": 284_000,
                        "tracknumber": 1,
                        "recording_mbid": "1a2b"
                    }
                }
            }]
        });
        assert_eq!(v, expected);
    }

    #[test]
    fn playing_now_has_no_timestamp_and_skips_empty_fields() {
        let t = ScrobbleTrack {
            album: Some("  ".into()),
            duration_ms: 0,
            track_number: None,
            mbid: None,
            ..track()
        };
        let v = listen_payload("playing_now", &[(&t, None)]);
        let listen = &v["payload"][0];
        assert!(listen.get("listened_at").is_none());
        let meta = &listen["track_metadata"];
        assert!(meta.get("release_name").is_none());
        let info = meta["additional_info"].as_object().unwrap();
        assert!(!info.contains_key("duration_ms"));
        assert!(!info.contains_key("tracknumber"));
        assert_eq!(info["media_player"], "ricercar");
    }

    #[test]
    fn import_payload_keeps_order() {
        let t = track();
        let v = listen_payload("import", &[(&t, Some(1)), (&t, Some(2))]);
        assert_eq!(v["listen_type"], "import");
        assert_eq!(v["payload"][1]["listened_at"], 2);
    }

    #[test]
    fn validate_response_parsing() {
        let ok: ValidateResponse = serde_json::from_str(
            r#"{"code":200,"message":"Token valid.","valid":true,"user_name":"pata"}"#,
        )
        .unwrap();
        assert_eq!(parse_validate(ok).as_deref(), Some("pata"));
        let bad: ValidateResponse =
            serde_json::from_str(r#"{"code":200,"message":"Token invalid.","valid":false}"#)
                .unwrap();
        assert_eq!(parse_validate(bad), None);
    }

    #[test]
    fn empty_token_is_not_configured() {
        let lb = ListenBrainz::new("  ");
        assert!(matches!(
            lb.playing_now(&track()),
            Err(OnlineError::NotConfigured)
        ));
        assert!(matches!(
            lb.validate_token(),
            Err(OnlineError::NotConfigured)
        ));
    }

    #[test]
    fn base_url_normalized() {
        let lb = ListenBrainz::new("t").with_base_url("http://localhost:8100/");
        assert_eq!(lb.base, "http://localhost:8100");
    }

    // Hits the real API; run with `LISTENBRAINZ_TOKEN=... cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_validate_token() {
        let token = std::env::var("LISTENBRAINZ_TOKEN").unwrap_or_default();
        let lb = ListenBrainz::new(token);
        println!("{:?}", lb.validate_token());
    }
}
