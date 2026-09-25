//! Radio Browser (<https://api.radio-browser.info/>): community directory of
//! internet radio stations, no key required.
//!
//! HLS stations (`.m3u8` playlists of segments) are excluded by default:
//! ricercar's decoder plays continuous streams (Icecast/Shoutcast MP3, AAC,
//! Ogg, FLAC) but has no HLS playlist/segment client. The API cannot filter
//! them server-side, so filtering happens after the fact and a page may hold
//! fewer than `limit` stations.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::http::{self, agent};
use crate::{OnlineError, Result};

pub const DISCOVERY_HOST: &str = "all.api.radio-browser.info";
pub const FALLBACK_HOST: &str = "de1.api.radio-browser.info";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawStation")]
pub struct Station {
    pub uuid: String,
    pub name: String,
    /// Direct stream URL (playlists already resolved by the directory).
    pub url_resolved: String,
    pub homepage: String,
    pub favicon: String,
    pub tags: Vec<String>,
    pub country: String,
    pub countrycode: String,
    pub language: String,
    pub codec: String,
    /// kbit/s, `0` if unknown.
    pub bitrate: u32,
    pub votes: u64,
    pub clickcount: u64,
    pub hls: bool,
}

/// Wire format: tags are comma-separated, `hls` is 0/1, fields may be null.
#[derive(Deserialize)]
struct RawStation {
    #[serde(alias = "uuid")]
    stationuuid: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url_resolved: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    favicon: Option<String>,
    #[serde(default)]
    tags: Option<TagsField>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    countrycode: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate: Option<u32>,
    #[serde(default)]
    votes: Option<u64>,
    #[serde(default)]
    clickcount: Option<u64>,
    #[serde(default)]
    hls: Option<FlagField>,
}

/// Accepts both the API's comma string and our own serialized list.
#[derive(Deserialize)]
#[serde(untagged)]
enum TagsField {
    Csv(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FlagField {
    Int(u8),
    Bool(bool),
}

impl From<RawStation> for Station {
    fn from(r: RawStation) -> Self {
        let tags = match r.tags {
            Some(TagsField::Csv(s)) => s
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(TagsField::List(v)) => v,
            None => Vec::new(),
        };
        let url_resolved = r
            .url_resolved
            .filter(|u| !u.is_empty())
            .or(r.url)
            .unwrap_or_default();
        Station {
            uuid: r.stationuuid,
            name: r.name.unwrap_or_default().trim().to_owned(),
            url_resolved,
            homepage: r.homepage.unwrap_or_default(),
            favicon: r.favicon.unwrap_or_default(),
            tags,
            country: r.country.unwrap_or_default(),
            countrycode: r.countrycode.unwrap_or_default(),
            language: r.language.unwrap_or_default(),
            codec: r.codec.unwrap_or_default(),
            bitrate: r.bitrate.unwrap_or(0),
            votes: r.votes.unwrap_or(0),
            clickcount: r.clickcount.unwrap_or(0),
            hls: matches!(r.hls, Some(FlagField::Int(1..) | FlagField::Bool(true))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationOrder {
    Name,
    Country,
    Language,
    Tags,
    Votes,
    Codec,
    Bitrate,
    ClickCount,
    ClickTrend,
    ChangeTimestamp,
    Random,
}

impl StationOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            StationOrder::Name => "name",
            StationOrder::Country => "country",
            StationOrder::Language => "language",
            StationOrder::Tags => "tags",
            StationOrder::Votes => "votes",
            StationOrder::Codec => "codec",
            StationOrder::Bitrate => "bitrate",
            StationOrder::ClickCount => "clickcount",
            StationOrder::ClickTrend => "clicktrend",
            StationOrder::ChangeTimestamp => "changetimestamp",
            StationOrder::Random => "random",
        }
    }
}

/// Parameters for `/json/stations/search`. `None`/empty fields are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationQuery {
    pub name: Option<String>,
    pub tag: Option<String>,
    /// ISO 3166-1 alpha-2.
    pub countrycode: Option<String>,
    pub language: Option<String>,
    pub order: Option<StationOrder>,
    pub reverse: bool,
    pub limit: u32,
    pub offset: u32,
    pub hidebroken: bool,
    /// Keep HLS stations (unplayable by ricercar; see module docs).
    pub include_hls: bool,
}

impl Default for StationQuery {
    fn default() -> Self {
        Self {
            name: None,
            tag: None,
            countrycode: None,
            language: None,
            order: None,
            reverse: false,
            limit: 100,
            offset: 0,
            hidebroken: true,
            include_hls: false,
        }
    }
}

impl StationQuery {
    pub(crate) fn params(&self) -> Vec<(&'static str, String)> {
        let mut p = Vec::new();
        let text = [
            ("name", &self.name),
            ("tag", &self.tag),
            ("countrycode", &self.countrycode),
            ("language", &self.language),
        ];
        for (k, v) in text {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                p.push((k, v.to_owned()));
            }
        }
        if let Some(o) = self.order {
            p.push(("order", o.as_str().to_owned()));
        }
        p.push(("reverse", self.reverse.to_string()));
        p.push(("limit", self.limit.to_string()));
        p.push(("offset", self.offset.to_string()));
        p.push(("hidebroken", self.hidebroken.to_string()));
        p
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tag {
    pub name: String,
    #[serde(default)]
    pub stationcount: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Country {
    pub name: String,
    /// ISO 3166-1 alpha-2.
    #[serde(alias = "countrycode", default)]
    pub iso_3166_1: String,
    #[serde(default)]
    pub stationcount: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioBrowser {
    host: String,
}

impl RadioBrowser {
    /// Picks a mirror from the server list, as the API docs ask clients to
    /// spread load; falls back to [`FALLBACK_HOST`] on any failure. Blocking.
    ///
    /// The docs suggest reverse-resolving `all.api.radio-browser.info`, which
    /// std cannot do, so we use the `/json/servers` endpoint instead.
    pub fn discover() -> Self {
        let url = format!("https://{DISCOVERY_HOST}/json/servers");
        match http::get_json::<Vec<ServerEntry>>(agent().get(&url)) {
            Ok(servers) => match pick_server(&servers, seed()) {
                Some(host) => Self::with_host(host),
                None => Self::with_host(FALLBACK_HOST),
            },
            Err(e) => {
                tracing::warn!(error = %e, "radio-browser server discovery failed, using fallback");
                Self::with_host(FALLBACK_HOST)
            }
        }
    }

    /// Uses a specific mirror host, e.g. `de1.api.radio-browser.info`.
    pub fn with_host(host: impl Into<String>) -> Self {
        Self { host: host.into() }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn search(&self, query: &StationQuery) -> Result<Vec<Station>> {
        let mut req = agent().get(&self.url("/json/stations/search"));
        for (k, v) in query.params() {
            req = req.query(k, &v);
        }
        Ok(filter_hls(http::get_json(req)?, query.include_hls))
    }

    pub fn top_clicked(&self, limit: u32) -> Result<Vec<Station>> {
        self.list(&format!("/json/stations/topclick/{limit}"))
    }

    pub fn top_voted(&self, limit: u32) -> Result<Vec<Station>> {
        self.list(&format!("/json/stations/topvote/{limit}"))
    }

    /// Looks a station up regardless of HLS, so saved favourites resolve.
    pub fn by_uuid(&self, uuid: &str) -> Result<Option<Station>> {
        let req = agent().get(&self.url(&format!("/json/stations/byuuid/{uuid}")));
        let stations: Vec<Station> = http::get_json(req)?;
        Ok(stations.into_iter().next())
    }

    /// Registers a play, as the API asks clients to do whenever a user starts
    /// a station (it feeds the popularity ranking). Returns the stream URL.
    pub fn count_click(&self, uuid: &str) -> Result<String> {
        let req = agent().get(&self.url(&format!("/json/url/{uuid}")));
        parse_click(http::get_json(req)?)
    }

    /// Most used tags, by station count.
    pub fn tags(&self, limit: u32) -> Result<Vec<Tag>> {
        let req = agent()
            .get(&self.url("/json/tags"))
            .query("order", "stationcount")
            .query("reverse", "true")
            .query("hidebroken", "true")
            .query("limit", &limit.to_string());
        http::get_json(req)
    }

    pub fn countries(&self) -> Result<Vec<Country>> {
        let req = agent()
            .get(&self.url("/json/countries"))
            .query("hidebroken", "true");
        http::get_json(req)
    }

    fn list(&self, path: &str) -> Result<Vec<Station>> {
        let req = agent().get(&self.url(path)).query("hidebroken", "true");
        Ok(filter_hls(http::get_json(req)?, false))
    }

    fn url(&self, path: &str) -> String {
        format!("https://{}{path}", self.host)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServerEntry {
    name: String,
}

/// Picks a server by `seed` among distinct host names.
fn pick_server(servers: &[ServerEntry], seed: u64) -> Option<&str> {
    let mut names: Vec<&str> = servers
        .iter()
        .map(|s| s.name.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return None;
    }
    Some(names[(seed % names.len() as u64) as usize])
}

/// Cheap randomness for load spreading; no need for a RNG dependency.
fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64 ^ d.as_secs())
}

fn filter_hls(stations: Vec<Station>, include_hls: bool) -> Vec<Station> {
    if include_hls {
        stations
    } else {
        stations.into_iter().filter(|s| !s.hls).collect()
    }
}

#[derive(Deserialize)]
struct ClickResponse {
    ok: bool,
    #[serde(default)]
    message: String,
    #[serde(default)]
    url: String,
}

fn parse_click(r: ClickResponse) -> Result<String> {
    if r.ok {
        Ok(r.url)
    } else {
        Err(OnlineError::Http {
            status: 404,
            body: r.message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Station> {
        serde_json::from_str(include_str!("../tests/fixtures/radio_stations.json")).unwrap()
    }

    #[test]
    fn station_parsing() {
        let stations = fixture();
        assert_eq!(stations.len(), 3);
        let fip = &stations[0];
        assert_eq!(fip.uuid, "d7e8f1a2-0000-4000-8000-000000000001");
        assert_eq!(fip.name, "FIP");
        assert_eq!(
            fip.url_resolved,
            "https://icecast.radiofrance.fr/fip-hifi.aac"
        );
        assert_eq!(fip.tags, ["eclectic", "jazz", "public radio"]);
        assert_eq!(fip.countrycode, "FR");
        assert_eq!(fip.codec, "AAC");
        assert_eq!(fip.bitrate, 192);
        assert_eq!(fip.votes, 12_000);
        assert_eq!(fip.clickcount, 950);
        assert!(!fip.hls);

        let rtl = &stations[1];
        assert!(rtl.hls);
        assert_eq!(rtl.tags, ["généraliste"]);

        // Nulls and a missing url_resolved fall back gracefully.
        let sparse = &stations[2];
        assert_eq!(sparse.url_resolved, "http://example.org/stream");
        assert!(sparse.tags.is_empty());
        assert_eq!(sparse.bitrate, 0);
        assert_eq!(sparse.name, "Sparse");
    }

    #[test]
    fn hls_excluded_by_default() {
        let playable = filter_hls(fixture(), false);
        assert_eq!(playable.len(), 2);
        assert!(playable.iter().all(|s| !s.hls));
        assert_eq!(filter_hls(fixture(), true).len(), 3);
    }

    #[test]
    fn station_serde_roundtrip() {
        let s = fixture().remove(0);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Station>(&json).unwrap(), s);
    }

    #[test]
    fn query_params() {
        let q = StationQuery {
            name: Some(" jazz ".into()),
            tag: Some(String::new()),
            countrycode: Some("FR".into()),
            order: Some(StationOrder::Votes),
            reverse: true,
            limit: 20,
            ..Default::default()
        };
        let params = q.params();
        let get = |k: &str| {
            params
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("name"), Some("jazz"));
        assert_eq!(get("tag"), None);
        assert_eq!(get("countrycode"), Some("FR"));
        assert_eq!(get("language"), None);
        assert_eq!(get("order"), Some("votes"));
        assert_eq!(get("reverse"), Some("true"));
        assert_eq!(get("limit"), Some("20"));
        assert_eq!(get("offset"), Some("0"));
        assert_eq!(get("hidebroken"), Some("true"));
    }

    #[test]
    fn default_query_hides_broken_and_hls() {
        let q = StationQuery::default();
        assert!(q.hidebroken);
        assert!(!q.include_hls);
    }

    #[test]
    fn server_picking() {
        let servers: Vec<ServerEntry> = serde_json::from_str(
            r#"[{"ip":"1.2.3.4","name":"de1.api.radio-browser.info"},
                {"ip":"::1","name":"de1.api.radio-browser.info"},
                {"ip":"5.6.7.8","name":"fi1.api.radio-browser.info"}]"#,
        )
        .unwrap();
        assert_eq!(pick_server(&servers, 0), Some("de1.api.radio-browser.info"));
        assert_eq!(pick_server(&servers, 1), Some("fi1.api.radio-browser.info"));
        assert_eq!(pick_server(&servers, 2), Some("de1.api.radio-browser.info"));
        assert_eq!(pick_server(&[], 7), None);
    }

    #[test]
    fn tags_and_countries_parsing() {
        let tags: Vec<Tag> =
            serde_json::from_str(r#"[{"name":"pop","stationcount":9000}]"#).unwrap();
        assert_eq!(tags[0].name, "pop");
        assert_eq!(tags[0].stationcount, 9000);
        let countries: Vec<Country> = serde_json::from_str(
            r#"[{"name":"Andorra","iso_3166_1":"AD","stationcount":12},
                {"name":"Old","countrycode":"XX","stationcount":1}]"#,
        )
        .unwrap();
        assert_eq!(countries[0].iso_3166_1, "AD");
        assert_eq!(countries[1].iso_3166_1, "XX");
    }

    #[test]
    fn click_parsing() {
        let ok: ClickResponse = serde_json::from_str(
            r#"{"ok":true,"message":"retrieved station url","stationuuid":"x","name":"FIP","url":"https://s/fip"}"#,
        )
        .unwrap();
        assert_eq!(parse_click(ok).unwrap(), "https://s/fip");
        let ko: ClickResponse =
            serde_json::from_str(r#"{"ok":false,"message":"did not find station"}"#).unwrap();
        assert!(parse_click(ko).is_err());
    }

    // Hits the real directory.
    #[test]
    #[ignore]
    fn live_top_clicked() {
        let rb = RadioBrowser::discover();
        let stations = rb.top_clicked(10).unwrap();
        assert!(stations.iter().all(|s| !s.hls));
    }
}
