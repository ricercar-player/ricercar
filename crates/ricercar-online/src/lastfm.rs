//! Last.fm scrobbling API 2.0 (<https://www.last.fm/api>), using the user's
//! own API account (`api_key` + `shared_secret`).
//!
//! Desktop auth flow:
//! 1. [`LastFm::get_token`], then open [`LastFm::auth_url`] in a browser;
//! 2. once the user approved, [`LastFm::get_session`] yields a session key
//!    that never expires; store it and pass it to the scrobbling calls.
//!    Before approval it fails with [`OnlineError::Auth`] (error 14), so it
//!    can be polled.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{self, agent};
use crate::scrobble::{Scrobble, ScrobbleTrack};
use crate::{OnlineError, Result, fsutil};

pub const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
pub const AUTH_URL: &str = "https://www.last.fm/api/auth/";
/// `track.scrobble` accepts at most 50 scrobbles per request.
pub const MAX_BATCH: usize = 50;

#[derive(Debug, Clone)]
pub struct LastFm {
    api_key: String,
    shared_secret: String,
}

/// An authorized session; `key` is long-lived and should be persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub key: String,
}

/// Outcome of a `track.scrobble` call, summed over batches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrobbleOutcome {
    pub accepted: u32,
    /// Filtered by Last.fm (e.g. timestamp too old, bad metadata); resending
    /// them would not help.
    pub ignored: u32,
}

type Params = Vec<(String, String)>;

impl LastFm {
    pub fn new(api_key: impl Into<String>, shared_secret: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into().trim().to_owned(),
            shared_secret: shared_secret.into().trim().to_owned(),
        }
    }

    pub fn get_token(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct R {
            token: String,
        }
        let v = self.call("auth.getToken", Vec::new())?;
        let r: R = serde_json::from_value(v).map_err(|e| OnlineError::Parse(e.to_string()))?;
        Ok(r.token)
    }

    /// Page where the user grants access to `token`.
    pub fn auth_url(&self, token: &str) -> String {
        format!("{AUTH_URL}?api_key={}&token={}", self.api_key, token)
    }

    pub fn get_session(&self, token: &str) -> Result<Session> {
        let v = self.call("auth.getSession", vec![("token".into(), token.into())])?;
        parse_session(v)
    }

    pub fn update_now_playing(&self, session_key: &str, track: &ScrobbleTrack) -> Result<()> {
        let mut params = vec![("sk".to_owned(), session_key.to_owned())];
        push_track_params(&mut params, track, None);
        self.call("track.updateNowPlaying", params)?;
        Ok(())
    }

    /// Scrobbles in batches of up to [`MAX_BATCH`].
    pub fn scrobble(&self, session_key: &str, scrobbles: &[Scrobble]) -> Result<ScrobbleOutcome> {
        let mut total = ScrobbleOutcome::default();
        for chunk in scrobbles.chunks(MAX_BATCH) {
            let mut params = vec![("sk".to_owned(), session_key.to_owned())];
            for (i, s) in chunk.iter().enumerate() {
                push_track_params(&mut params, &s.track, Some((i, s.started_at_unix)));
            }
            let outcome = parse_scrobble_outcome(&self.call("track.scrobble", params)?);
            total.accepted += outcome.accepted;
            total.ignored += outcome.ignored;
        }
        Ok(total)
    }

    /// Signs and POSTs a method call, returning the JSON body.
    fn call(&self, method: &str, mut params: Params) -> Result<Value> {
        if self.api_key.is_empty() || self.shared_secret.is_empty() {
            return Err(OnlineError::NotConfigured);
        }
        params.push(("method".into(), method.into()));
        params.push(("api_key".into(), self.api_key.clone()));
        let sig = api_signature(&params, &self.shared_secret);
        params.push(("api_sig".into(), sig));
        params.push(("format".into(), "json".into()));

        let form: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let body = match http::send(agent().post(API_URL).send_form(&form)) {
            Ok(resp) => http::body_string(resp)?,
            Err(OnlineError::Http { status, body }) => {
                return Err(api_error(&body).unwrap_or(OnlineError::Http { status, body }));
            }
            Err(e) => return Err(e),
        };
        // Last.fm sometimes reports failures with HTTP 200.
        if let Some(err) = api_error(&body) {
            return Err(err);
        }
        http::parse_json(&body)
    }
}

/// `api_sig`: md5 of all parameters sorted by name, concatenated as
/// `name` + `value`, followed by the shared secret. `format` and `callback`
/// are not signed.
pub fn api_signature(params: &[(String, String)], shared_secret: &str) -> String {
    let mut sorted: Vec<_> = params
        .iter()
        .filter(|(k, _)| k != "format" && k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    s.push_str(shared_secret);
    fsutil::md5_hex(s.as_bytes())
}

/// Appends track fields; `batch` = `(index, timestamp)` for `track.scrobble`
/// array parameters (`artist[i]`...), `None` for plain names.
fn push_track_params(params: &mut Params, t: &ScrobbleTrack, batch: Option<(usize, i64)>) {
    let name = |base: &str| match batch {
        Some((i, _)) => format!("{base}[{i}]"),
        None => base.to_owned(),
    };
    let mut push = |base: &str, value: String| params.push((name(base), value));
    push("artist", t.artist.clone());
    push("track", t.title.clone());
    if let Some((_, ts)) = batch {
        push("timestamp", ts.to_string());
    }
    let optional = [
        ("album", &t.album),
        ("albumArtist", &t.album_artist),
        ("mbid", &t.mbid),
    ];
    for (key, value) in optional {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            push(key, v.to_owned());
        }
    }
    if let Some(n) = t.track_number {
        push("trackNumber", n.to_string());
    }
    if t.duration_ms > 0 {
        push("duration", (t.duration_ms / 1000).to_string());
    }
}

/// Maps a Last.fm `{"error": n, "message": ...}` body to an error, if it is one.
fn api_error(body: &str) -> Option<OnlineError> {
    let v: Value = serde_json::from_str(body).ok()?;
    let code = v.get("error")?.as_u64()?;
    let message = v.get("message").and_then(Value::as_str).unwrap_or_default();
    let text = format!("Last.fm error {code}: {message}");
    Some(match code {
        // invalid/expired/unauthorized token, invalid session, key or signature, suspended key
        4 | 9 | 10 | 13 | 14 | 15 | 26 => OnlineError::Auth(text),
        // service offline, temporarily unavailable, rate limited: retry later
        11 | 16 | 29 => OnlineError::Http {
            status: 503,
            body: text,
        },
        _ => OnlineError::Http {
            status: 400,
            body: text,
        },
    })
}

fn parse_session(v: Value) -> Result<Session> {
    #[derive(Deserialize)]
    struct R {
        session: Session,
    }
    let r: R = serde_json::from_value(v).map_err(|e| OnlineError::Parse(e.to_string()))?;
    Ok(r.session)
}

fn parse_scrobble_outcome(v: &Value) -> ScrobbleOutcome {
    // Counts come as numbers or strings depending on the endpoint's mood.
    let count = |key: &str| {
        let n = &v["scrobbles"]["@attr"][key];
        n.as_u64()
            .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0) as u32
    };
    ScrobbleOutcome {
        accepted: count("accepted"),
        ignored: count("ignored"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Params {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn signature_simple_vector() {
        // md5("api_keyxxxxxxxxmethodauth.getSessiontokenyyyyyyyyilovecher"),
        // computed with `printf '%s' ... | md5sum`.
        let params = p(&[
            ("token", "yyyyyyyy"),
            ("method", "auth.getSession"),
            ("api_key", "xxxxxxxx"),
            ("format", "json"),
            ("callback", "cb"),
        ]);
        assert_eq!(
            api_signature(&params, "ilovecher"),
            "50b30c3b138fd7e02ae9d9f4b3e84c30"
        );
    }

    #[test]
    fn signature_batch_utf8_vector() {
        // md5 (via `printf '%s' ... | md5sum`) of the sorted string:
        // "album[0]Ç Vapi_keyKartist[0]Björkartist[1]Sigur Rósmethodtrack.scrobble
        //  sksesstimestamp[0]100timestamp[1]200track[0]Jógatrack[1]HoppípollaS"
        let params = p(&[
            ("method", "track.scrobble"),
            ("api_key", "K"),
            ("sk", "sess"),
            ("artist[0]", "Björk"),
            ("track[0]", "Jóga"),
            ("timestamp[0]", "100"),
            ("album[0]", "Ç V"),
            ("artist[1]", "Sigur Rós"),
            ("track[1]", "Hoppípolla"),
            ("timestamp[1]", "200"),
            ("format", "json"),
        ]);
        assert_eq!(
            api_signature(&params, "S"),
            "efb1592d756c61e8b61c944c38eb64ce"
        );
    }

    fn track() -> ScrobbleTrack {
        ScrobbleTrack {
            artist: "Björk".into(),
            title: "Jóga".into(),
            album: Some("Homogenic".into()),
            album_artist: None,
            duration_ms: 305_900,
            track_number: Some(2),
            mbid: Some(String::new()),
        }
    }

    #[test]
    fn batch_params_are_indexed() {
        let mut params = Vec::new();
        push_track_params(&mut params, &track(), Some((3, 1_000)));
        assert_eq!(
            params,
            p(&[
                ("artist[3]", "Björk"),
                ("track[3]", "Jóga"),
                ("timestamp[3]", "1000"),
                ("album[3]", "Homogenic"),
                ("trackNumber[3]", "2"),
                ("duration[3]", "305"),
            ])
        );
    }

    #[test]
    fn now_playing_params_are_plain() {
        let mut params = Vec::new();
        let t = ScrobbleTrack {
            album_artist: Some("Björk".into()),
            duration_ms: 0,
            track_number: None,
            ..track()
        };
        push_track_params(&mut params, &t, None);
        assert_eq!(
            params,
            p(&[
                ("artist", "Björk"),
                ("track", "Jóga"),
                ("album", "Homogenic"),
                ("albumArtist", "Björk"),
            ])
        );
    }

    #[test]
    fn auth_url_format() {
        let fm = LastFm::new("KEY", "SECRET");
        assert_eq!(
            fm.auth_url("TOK"),
            "https://www.last.fm/api/auth/?api_key=KEY&token=TOK"
        );
    }

    #[test]
    fn session_parsing() {
        let v = serde_json::json!({"session": {"subscriber": 0, "name": "pata", "key": "d580d57f32848f5dcf574d1ce18d78b2"}});
        assert_eq!(
            parse_session(v).unwrap(),
            Session {
                name: "pata".into(),
                key: "d580d57f32848f5dcf574d1ce18d78b2".into()
            }
        );
        assert!(parse_session(serde_json::json!({"nope": 1})).is_err());
    }

    #[test]
    fn scrobble_outcome_parsing() {
        let v: Value = serde_json::from_str(
            r#"{"scrobbles":{"scrobble":[],"@attr":{"accepted":2,"ignored":"1"}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_scrobble_outcome(&v),
            ScrobbleOutcome {
                accepted: 2,
                ignored: 1
            }
        );
        assert_eq!(
            parse_scrobble_outcome(&Value::Null),
            ScrobbleOutcome::default()
        );
    }

    #[test]
    fn api_error_mapping() {
        let e = api_error(r#"{"error":14,"message":"Unauthorized Token"}"#).unwrap();
        assert!(matches!(e, OnlineError::Auth(m) if m.contains("14")));
        let e = api_error(r#"{"error":29,"message":"Rate limit exceeded"}"#).unwrap();
        assert!(e.is_transient());
        let e = api_error(r#"{"error":6,"message":"Invalid parameters"}"#).unwrap();
        assert!(matches!(e, OnlineError::Http { status: 400, .. }));
        assert!(api_error(r#"{"token":"abc"}"#).is_none());
        assert!(api_error("<html>").is_none());
    }

    #[test]
    fn missing_credentials_are_not_configured() {
        let fm = LastFm::new("", "secret");
        assert!(matches!(fm.get_token(), Err(OnlineError::NotConfigured)));
    }

    // Hits the real API; run with LASTFM_API_KEY / LASTFM_SECRET set and `--ignored`.
    #[test]
    #[ignore]
    fn live_get_token() {
        let fm = LastFm::new(
            std::env::var("LASTFM_API_KEY").unwrap_or_default(),
            std::env::var("LASTFM_SECRET").unwrap_or_default(),
        );
        let token = fm.get_token().unwrap();
        println!("{}", fm.auth_url(&token));
    }
}
