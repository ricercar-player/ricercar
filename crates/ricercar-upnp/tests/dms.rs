//! MediaServer: ContentDirectory hierarchy, paging, metadata, search, and
//! HTTP serving of library files and art.

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;

fn browse(port: u16, id: &str, flag: &str, start: u32, count: u32) -> String {
    soap(
        port,
        "cd",
        CD,
        "Browse",
        &[
            ("ObjectID", id),
            ("BrowseFlag", flag),
            ("Filter", "*"),
            ("StartingIndex", &start.to_string()),
            ("RequestCount", &count.to_string()),
            ("SortCriteria", ""),
        ],
    )
}

fn children(port: u16, id: &str) -> String {
    browse(port, id, "BrowseDirectChildren", 0, 0)
}

/// `attr="value"` of the first element carrying it after `from`.
fn attr(xml: &str, from: &str, name: &str) -> String {
    let at = xml
        .find(from)
        .unwrap_or_else(|| panic!("{from} not in {xml}"));
    let rest = &xml[at..];
    let key = format!(" {name}=\"");
    let s = rest
        .find(&key)
        .unwrap_or_else(|| panic!("no {name} in {rest}"))
        + key.len();
    rest[s..].split('"').next().unwrap().to_string()
}

/// Object ids of containers/items in a DIDL result.
fn ids(didl: &str) -> Vec<String> {
    didl.split(" id=\"")
        .skip(1)
        .map(|s| unescape(s.split('"').next().unwrap()))
        .collect()
}

fn media_path(didl: &str) -> String {
    let res = didl.split("<res ").nth(1).expect("res");
    let url = res.split('>').nth(1).unwrap().split('<').next().unwrap();
    let url = unescape(url);
    url[url.find("/media/").unwrap()..].to_string()
}

#[test]
fn server_desc() {
    let r = rig("dms-desc", true);
    let (_, body) = get(r.port(), "/server.xml", "");
    let desc = String::from_utf8(body).unwrap();
    assert!(desc.contains("MediaServer"));
    assert!(desc.contains("ContentDirectory"));
    let (_, scpd) = get(r.port(), "/svc/cd.xml", "");
    let scpd = String::from_utf8(scpd).unwrap();
    assert!(scpd.contains("<name>Search</name>"));
    assert!(scpd.contains("<name>Browse</name>"));
    let cms = soap(
        r.port(),
        "scms",
        "urn:schemas-upnp-org:service:ConnectionManager:1",
        "GetProtocolInfo",
        &[],
    );
    assert!(field(&cms, "Source").contains("http-get:*:audio/flac:"));
    let id = soap(r.port(), "cd", CD, "GetSystemUpdateID", &[]);
    assert_eq!(field(&id, "Id"), (r.ctl.lib.revision() as u32).to_string());
}

#[test]
fn hierarchy_paging_metadata() {
    let r = rig("dms-tree", true);
    let p = r.port();

    let root = children(p, "0");
    assert_eq!(field(&root, "TotalMatches"), "6");
    let didl = field(&root, "Result");
    assert_eq!(
        ids(&didl),
        [
            "albums",
            "artists",
            "genres",
            "tracks",
            "playlists",
            "recent"
        ]
    );
    assert_eq!(attr(&didl, "id=\"tracks\"", "childCount"), "2");
    assert_eq!(attr(&didl, "id=\"albums\"", "parentID"), "0");

    // Albums → album (with art) → tracks.
    let albums = field(&children(p, "albums"), "Result");
    assert!(albums.contains("object.container.album.musicAlbum"));
    assert!(albums.contains("<upnp:albumArtURI"), "{albums}");
    let album_id = ids(&albums)[0].clone();
    assert!(album_id.starts_with("albums/"));
    assert_eq!(attr(&albums, "<container", "childCount"), "2");

    let tracks = children(p, &album_id);
    assert_eq!(field(&tracks, "TotalMatches"), "2");
    let didl = field(&tracks, "Result");
    let items = ids(&didl);
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i.starts_with(&format!("{album_id}/"))));
    assert_eq!(attr(&didl, "<item", "parentID"), album_id);
    assert!(didl.contains("object.item.audioItem.musicTrack"));
    assert!(didl.contains("<upnp:albumArtURI"));
    // res carries the audio properties and the right MIME.
    let hi = didl
        .split("<item ")
        .find(|s| s.contains("tone_24_96"))
        .expect("24/96 item");
    assert!(hi.contains("protocolInfo=\"http-get:*:audio/flac:"), "{hi}");
    assert!(hi.contains("sampleFrequency=\"96000\""), "{hi}");
    assert!(hi.contains("bitsPerSample=\"24\""), "{hi}");
    assert!(hi.contains("nrAudioChannels=\"2\""), "{hi}");
    assert!(hi.contains("duration=\"0:00:02.000\""), "{hi}");
    let size = std::fs::metadata(fixtures().join("tone_24_96.flac"))
        .unwrap()
        .len();
    assert!(hi.contains(&format!("size=\"{size}\"")), "{hi}");

    // Paging over "All tracks".
    let page = browse(p, "tracks", "BrowseDirectChildren", 1, 1);
    assert_eq!(field(&page, "NumberReturned"), "1");
    assert_eq!(field(&page, "TotalMatches"), "2");
    let all = ids(&field(&children(p, "tracks"), "Result"));
    assert_eq!(ids(&field(&page, "Result")), [all[1].clone()]);
    let past = browse(p, "tracks", "BrowseDirectChildren", 5, 10);
    assert_eq!(field(&past, "NumberReturned"), "0");
    assert_eq!(field(&past, "TotalMatches"), "2");

    // BrowseMetadata for any id.
    let meta = field(&browse(p, "0", "BrowseMetadata", 0, 0), "Result");
    assert_eq!(attr(&meta, "<container", "parentID"), "-1");
    assert_eq!(attr(&meta, "<container", "childCount"), "6");
    let meta = browse(p, &album_id, "BrowseMetadata", 0, 0);
    assert_eq!(field(&meta, "TotalMatches"), "1");
    let meta = field(&meta, "Result");
    assert_eq!(ids(&meta), std::slice::from_ref(&album_id));
    assert_eq!(attr(&meta, "<container", "parentID"), "albums");
    let meta = field(&browse(p, &items[0], "BrowseMetadata", 0, 0), "Result");
    assert_eq!(ids(&meta), [items[0].clone()]);
    assert_eq!(attr(&meta, "<item", "parentID"), album_id);
    let meta = field(&browse(p, &all[0], "BrowseMetadata", 0, 0), "Result");
    assert_eq!(attr(&meta, "<item", "parentID"), "tracks");
    let meta = field(&browse(p, "recent", "BrowseMetadata", 0, 0), "Result");
    assert!(meta.contains("Recently added"));

    // Recently added mirrors the album.
    let recent = field(&children(p, "recent"), "Result");
    assert_eq!(ids(&recent).len(), 1);
    assert_eq!(attr(&recent, "<container", "parentID"), "recent");

    // Unknown objects.
    let bad = browse(p, "albums/nope", "BrowseMetadata", 0, 0);
    assert_eq!(fault_code(&bad), Some(701), "{bad}");
    assert_eq!(fault_code(&children(p, "zzz")), Some(701));
    assert_eq!(fault_code(&children(p, &items[0])), Some(710));
}

#[test]
fn search_tracks_and_albums() {
    let r = rig("dms-search", true);
    let p = r.port();
    let caps = soap(p, "cd", CD, "GetSearchCapabilities", &[]);
    assert!(field(&caps, "SearchCaps").contains("dc:title"));
    let search = |criteria: &str| {
        soap(
            p,
            "cd",
            CD,
            "Search",
            &[
                ("ContainerID", "0"),
                ("SearchCriteria", criteria),
                ("Filter", "*"),
                ("StartingIndex", "0"),
                ("RequestCount", "0"),
                ("SortCriteria", ""),
            ],
        )
    };
    let t = r.ctl.lib.tracks(ricercar_core::TrackSort::Title);
    let word: String = t[0]
        .title
        .split(|c: char| !c.is_alphanumeric())
        .find(|w| w.len() >= 3)
        .unwrap()
        .to_string();

    let res = search(&format!(
        "upnp:class derivedfrom \"object.item.audioItem\" and dc:title contains \"{word}\""
    ));
    let didl = field(&res, "Result");
    assert!(
        field(&res, "TotalMatches").parse::<u32>().unwrap() >= 1,
        "{res}"
    );
    assert!(didl.contains("<item "), "{didl}");
    assert!(!didl.contains("<container "), "{didl}");
    assert!(didl.contains("/media/"));

    let none = search("dc:title contains \"zzqqxxnothing\"");
    assert_eq!(field(&none, "TotalMatches"), "0");

    // Class-only criteria: every track.
    let all = search("upnp:class derivedfrom \"object.item.audioItem\"");
    assert_eq!(field(&all, "TotalMatches"), "2");

    let albums = search("upnp:class = \"object.container.album.musicAlbum\"");
    let didl = field(&albums, "Result");
    assert!(
        didl.contains("musicAlbum") && !didl.contains("<item "),
        "{didl}"
    );

    let bad = search("dc:title contains");
    assert_eq!(fault_code(&bad), Some(708));
}

#[test]
fn media_streaming_range_and_head() {
    let r = rig("dms-media", true);
    let p = r.port();
    let album_id = ids(&field(&children(p, "albums"), "Result"))[0].clone();
    let didl = field(&children(p, &album_id), "Result");
    let path = media_path(&didl);
    let (head, bytes) = get(p, &path, "");
    assert!(head.contains("200 OK"), "{head}");
    assert!(head.contains("ACCEPT-RANGES: bytes"), "{head}");
    assert!(head.contains("transferMode.dlna.org: Streaming"), "{head}");
    assert!(
        head.contains("contentFeatures.dlna.org: DLNA.ORG_OP=01"),
        "{head}"
    );
    assert!(head.contains("CONTENT-TYPE: audio/flac"), "{head}");
    let file = ["tone_16_441.flac", "tone_24_96.flac"]
        .iter()
        .map(|f| std::fs::read(fixtures().join(f)).unwrap())
        .find(|b| *b == bytes)
        .expect("served bytes match a fixture");
    assert!(head.contains(&format!("CONTENT-LENGTH: {}", file.len())));

    let (head, body) = get(p, &path, "Range: bytes=0-9\r\n");
    assert!(head.contains("206 Partial Content"), "{head}");
    assert!(head.contains(&format!("CONTENT-RANGE: bytes 0-9/{}", file.len())));
    assert_eq!(body, &file[..10]);

    let (head, body) = get(p, &path, "Range: bytes=-5\r\n");
    assert!(head.contains("206"), "{head}");
    assert_eq!(body, &file[file.len() - 5..]);

    let (head, _) = get(p, &path, &format!("Range: bytes={}-\r\n", file.len() + 10));
    assert!(head.contains("416"), "{head}");

    let (head, body) = raw(
        p,
        format!("HEAD {path} HTTP/1.1\r\nHOST: x\r\n\r\n").as_bytes(),
    );
    assert!(head.contains("200 OK"), "{head}");
    assert!(head.contains(&format!("CONTENT-LENGTH: {}", file.len())));
    assert!(body.is_empty(), "HEAD returned {} body bytes", body.len());
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[test]
fn media_rejects_foreign_paths() {
    let r = rig("dms-sec", true);
    let (head, _) = get(
        r.port(),
        &format!("/media/{}", pct("file:///etc/passwd")),
        "",
    );
    assert!(head.contains("404"), "{head}");
    let (head, _) = get(r.port(), &format!("/media/{}", pct("/etc/passwd")), "");
    assert!(head.contains("404"), "{head}");
}

#[test]
fn art_never_fetches_remote_urls() {
    let r = rig("dms-art", true);
    // A "victim" server: any connection to it would be the SSRF.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let victim = l.local_addr().unwrap().port();
    let hit = Arc::new(AtomicBool::new(false));
    let hit2 = hit.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            if s.is_ok() {
                hit2.store(true, Ordering::SeqCst);
            }
        }
    });
    // Even when that URL is a queue item's cover hint (the audio URI
    // itself points elsewhere, so any hit on the victim is an art fetch).
    let audio = "http://127.0.0.1:9/a.flac".to_string();
    r.ctl.set_remote(
        &audio,
        Some(ricercar_core::TrackInfo {
            uri: audio.clone(),
            cover: Some(format!("http://127.0.0.1:{victim}/cover.jpg")),
            ..Default::default()
        }),
    );
    r.ctl.stop();
    for u in [
        format!("http://127.0.0.1:{victim}/cover.jpg"),
        audio.clone(),
        "file:///etc/passwd".to_string(),
    ] {
        let (head, _) = get(r.port(), &format!("/art?u={}", pct(&u)), "");
        assert!(head.contains("404"), "{u}: {head}");
    }
    let (head, _) = get(r.port(), "/art?a=nope", "");
    assert!(head.contains("404"), "{head}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!hit.load(Ordering::SeqCst), "renderer fetched a remote URL");
}
