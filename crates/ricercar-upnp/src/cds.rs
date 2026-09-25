//! ContentDirectory: the local library as a browsable/searchable tree.
//!
//! Object ids are stable paths of percent-encoded segments:
//! `0` · `albums` · `albums/<album>` · `albums/<album>/<track path>` ·
//! `artists/<name>/<album>/<track path>` · `genres/<genre>/<album>/…` ·
//! `tracks/<track path>` · `playlists/<id>/<position>` · `recent/<album>/…`.

use ricercar_core::{Album, AlbumSort, Artist, Genre, Playlist, Track, TrackInfo, TrackSort, meta};

use crate::media::{self, pct_decode, pct_encode};
use crate::soap::{Args, Reply};
use crate::{Renderer, Svc, didl, xml};

const RECENT_ALBUMS: usize = 50;

const TOPS: [(&str, &str); 6] = [
    ("albums", "Albums"),
    ("artists", "Artists"),
    ("genres", "Genres"),
    ("tracks", "All tracks"),
    ("playlists", "Playlists"),
    ("recent", "Recently added"),
];

pub const SEARCH_CAPS: &str = "dc:title,dc:creator,upnp:artist,upnp:album,upnp:genre,upnp:class";

/// A resolved object id.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Root,
    Top(&'static str),
    Artist(String),
    Genre(String),
    Playlist(i64),
    Album { parent: String, aid: String },
    Track { parent: String, path: String },
    PlaylistItem { pid: i64, pos: usize },
}

fn id_of(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| pct_encode(p))
        .collect::<Vec<_>>()
        .join("/")
}

fn top_name(t: &str) -> Option<&'static str> {
    TOPS.iter().find(|(k, _)| *k == t).map(|(k, _)| *k)
}

fn parse_id(id: &str) -> Option<Node> {
    if id == "0" {
        return Some(Node::Root);
    }
    let segs: Vec<String> = id.split('/').map(pct_decode).collect::<Option<Vec<_>>>()?;
    let s: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    let top = top_name(s.first()?)?;
    let album = |parent: &[&str], aid: &str| Node::Album {
        parent: id_of(parent),
        aid: aid.to_string(),
    };
    let track = |parent: &[&str], path: &str| Node::Track {
        parent: id_of(parent),
        path: path.to_string(),
    };
    Some(match (top, &s[1..]) {
        (_, []) => Node::Top(top),
        ("albums" | "recent", [aid]) => album(&[top], aid),
        ("albums" | "recent", [aid, path]) => track(&[top, aid], path),
        ("artists", [name]) => Node::Artist(name.to_string()),
        ("genres", [g]) => Node::Genre(g.to_string()),
        ("artists" | "genres", [k, aid]) => album(&[top, k], aid),
        ("artists" | "genres", [k, aid, path]) => track(&[top, k, aid], path),
        ("tracks", [path]) => track(&[top], path),
        ("playlists", [pid]) => Node::Playlist(pid.parse().ok()?),
        ("playlists", [pid, pos]) => Node::PlaylistItem {
            pid: pid.parse().ok()?,
            pos: pos.parse().ok()?,
        },
        _ => return None,
    })
}

/// One DIDL object, ready to render.
enum Entry {
    Root,
    Top(&'static str, &'static str, usize),
    Artist(Artist),
    Genre(Genre),
    Playlist(Playlist),
    Album { parent: String, album: Album },
    Track { parent: String, track: Track },
    PlaylistItem { pid: i64, pos: usize, track: Track },
}

fn container(
    id: &str,
    parent: &str,
    count: usize,
    title: &str,
    class: &str,
    extra: &str,
) -> String {
    format!(
        "<container id=\"{}\" parentID=\"{}\" childCount=\"{count}\" restricted=\"1\" searchable=\"1\"><dc:title>{}</dc:title>{extra}<upnp:class>{class}</upnp:class></container>",
        xml::escape(id),
        xml::escape(parent),
        xml::escape(title),
    )
}

fn art_tag(url: &str) -> String {
    format!(
        "<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">{}</upnp:albumArtURI>",
        xml::escape(url)
    )
}

fn track_art(base: &str, path: &str) -> String {
    media::track_art_url(base, &meta::file_uri(std::path::Path::new(path)))
}

fn track_item(id: &str, parent: &str, t: &Track, base: &str) -> String {
    let info = TrackInfo::from(t);
    let mime = media::mime_of(&t.path);
    let size = std::fs::metadata(&t.path).ok().map(|m| m.len());
    didl::ItemXml {
        id,
        parent,
        info: &info,
        res_url: &media::media_url(base, &t.uri),
        protocol_info: &media::protocol_info(mime),
        size,
        channels: t.channels,
        bitrate: t.bitrate.map(|k| k * 1000 / 8),
        art_url: Some(&media::track_art_url(base, &t.uri)),
    }
    .render()
}

impl Entry {
    fn id(&self) -> String {
        match self {
            Entry::Root => "0".into(),
            Entry::Top(k, _, _) => k.to_string(),
            Entry::Artist(a) => id_of(&["artists", &a.name]),
            Entry::Genre(g) => id_of(&["genres", &g.name]),
            Entry::Playlist(p) => id_of(&["playlists", &p.id.to_string()]),
            Entry::Album { parent, album } => format!("{parent}/{}", pct_encode(&album.id)),
            Entry::Track { parent, track } => format!("{parent}/{}", pct_encode(&track.path)),
            Entry::PlaylistItem { pid, pos, .. } => {
                id_of(&["playlists", &pid.to_string(), &pos.to_string()])
            }
        }
    }

    fn parent(&self) -> String {
        match self {
            Entry::Root => "-1".into(),
            Entry::Top(..) => "0".into(),
            Entry::Artist(_) => "artists".into(),
            Entry::Genre(_) => "genres".into(),
            Entry::Playlist(_) => "playlists".into(),
            Entry::Album { parent, .. } | Entry::Track { parent, .. } => parent.clone(),
            Entry::PlaylistItem { pid, .. } => id_of(&["playlists", &pid.to_string()]),
        }
    }

    fn render(&self, base: &str) -> String {
        let id = self.id();
        let parent = self.parent();
        match self {
            Entry::Root => container(&id, &parent, TOPS.len(), "Ricercar", "object.container", ""),
            Entry::Top(_, title, n) => container(&id, &parent, *n, title, "object.container", ""),
            Entry::Artist(a) => {
                let mut extra = String::new();
                if !a.cover_path.is_empty() {
                    extra.push_str(&art_tag(&track_art(base, &a.cover_path)));
                }
                container(
                    &id,
                    &parent,
                    a.album_count as usize,
                    &a.name,
                    "object.container.person.musicArtist",
                    &extra,
                )
            }
            Entry::Genre(g) => container(
                &id,
                &parent,
                g.album_count as usize,
                &g.name,
                "object.container.genre.musicGenre",
                "",
            ),
            Entry::Playlist(p) => {
                let extra = p
                    .cover_path
                    .as_deref()
                    .map(|c| art_tag(&track_art(base, c)))
                    .unwrap_or_default();
                container(
                    &id,
                    &parent,
                    p.track_count as usize,
                    &p.name,
                    "object.container.playlistContainer",
                    &extra,
                )
            }
            Entry::Album { album: a, .. } => {
                let e = xml::escape(&a.artist);
                let mut extra =
                    format!("<upnp:artist>{e}</upnp:artist><dc:creator>{e}</dc:creator>");
                if let Some(y) = a.year {
                    extra.push_str(&format!("<dc:date>{y:04}-01-01</dc:date>"));
                }
                if let Some(g) = &a.genre {
                    extra.push_str(&format!("<upnp:genre>{}</upnp:genre>", xml::escape(g)));
                }
                extra.push_str(&art_tag(&media::album_art_url(base, &a.id)));
                container(
                    &id,
                    &parent,
                    a.track_count as usize,
                    &a.title,
                    "object.container.album.musicAlbum",
                    &extra,
                )
            }
            Entry::Track { track, .. } | Entry::PlaylistItem { track, .. } => {
                track_item(&id, &parent, track, base)
            }
        }
    }
}

impl Renderer {
    pub fn system_update_id(&self) -> u32 {
        self.ctl.lib.revision() as u32
    }

    fn top_count(&self, top: &str) -> usize {
        let lib = &self.ctl.lib;
        match top {
            "albums" => lib.albums(AlbumSort::Title).len(),
            "artists" => lib.artists().len(),
            "genres" => lib.genres().len(),
            "tracks" => lib.count() as usize,
            "playlists" => lib.playlists().len(),
            "recent" => lib.recently_added_albums(RECENT_ALBUMS).len(),
            _ => 0,
        }
    }

    fn children(&self, node: &Node) -> Option<Vec<Entry>> {
        let lib = &self.ctl.lib;
        let albums = |parent: String, v: Vec<Album>| {
            v.into_iter()
                .map(|album| Entry::Album {
                    parent: parent.clone(),
                    album,
                })
                .collect::<Vec<_>>()
        };
        Some(match node {
            Node::Root => TOPS
                .iter()
                .map(|(k, t)| Entry::Top(k, t, self.top_count(k)))
                .collect(),
            Node::Top("albums") => albums("albums".into(), lib.albums(AlbumSort::Title)),
            Node::Top("recent") => {
                albums("recent".into(), lib.recently_added_albums(RECENT_ALBUMS))
            }
            Node::Top("artists") => lib.artists().into_iter().map(Entry::Artist).collect(),
            Node::Top("genres") => lib.genres().into_iter().map(Entry::Genre).collect(),
            Node::Top("playlists") => lib.playlists().into_iter().map(Entry::Playlist).collect(),
            Node::Top("tracks") => lib
                .tracks(TrackSort::Artist)
                .into_iter()
                .map(|track| Entry::Track {
                    parent: "tracks".into(),
                    track,
                })
                .collect(),
            Node::Top(_) => return None,
            Node::Artist(name) => albums(id_of(&["artists", name]), lib.artist_albums(name).0),
            Node::Genre(g) => albums(id_of(&["genres", g]), lib.genre_albums(g)),
            Node::Playlist(pid) => lib
                .playlist_tracks(*pid)
                .into_iter()
                .enumerate()
                .map(|(pos, track)| Entry::PlaylistItem {
                    pid: *pid,
                    pos,
                    track,
                })
                .collect(),
            Node::Album { parent, aid } => {
                let me = format!("{parent}/{}", pct_encode(aid));
                lib.album_tracks(aid)
                    .into_iter()
                    .map(|track| Entry::Track {
                        parent: me.clone(),
                        track,
                    })
                    .collect()
            }
            Node::Track { .. } | Node::PlaylistItem { .. } => Vec::new(),
        })
    }

    fn metadata(&self, node: &Node) -> Option<Entry> {
        let lib = &self.ctl.lib;
        Some(match node {
            Node::Root => Entry::Root,
            Node::Top(k) => {
                let title = TOPS.iter().find(|(t, _)| t == k)?.1;
                Entry::Top(k, title, self.top_count(k))
            }
            Node::Artist(name) => Entry::Artist(
                lib.artists()
                    .into_iter()
                    .find(|a| a.name.eq_ignore_ascii_case(name))?,
            ),
            Node::Genre(g) => Entry::Genre(
                lib.genres()
                    .into_iter()
                    .find(|x| x.name.eq_ignore_ascii_case(g))?,
            ),
            Node::Playlist(pid) => Entry::Playlist(lib.playlist(*pid)?),
            Node::Album { parent, aid } => Entry::Album {
                parent: parent.clone(),
                album: lib.album(aid)?,
            },
            Node::Track { parent, path } => {
                let track = lib.track(path)?;
                // The track must really live in the container named by its id.
                if let Some(Node::Album { aid, .. }) = parse_id(parent)
                    && track.album_id != aid
                {
                    return None;
                }
                Entry::Track {
                    parent: parent.clone(),
                    track,
                }
            }
            Node::PlaylistItem { pid, pos } => Entry::PlaylistItem {
                pid: *pid,
                pos: *pos,
                track: lib.playlist_tracks(*pid).into_iter().nth(*pos)?,
            },
        })
    }

    pub fn cd(&self, action: &str, args: &Args, base: &str) -> Reply {
        let ns = Svc::Cd.urn();
        let update_id = self.system_update_id().to_string();
        let ok = |out: &[(&str, &str)]| Reply::ok(action, ns, out);
        match action {
            "GetSearchCapabilities" => ok(&[("SearchCaps", SEARCH_CAPS)]),
            "GetSortCapabilities" => ok(&[("SortCaps", "")]),
            "GetSystemUpdateID" => ok(&[("Id", &update_id)]),
            "Browse" => {
                let Some(node) = parse_id(args.get("ObjectID").trim()) else {
                    return Reply::Err(701, "No such object");
                };
                let start = args.num::<usize>("StartingIndex").unwrap_or(0);
                let count = args.num::<usize>("RequestCount").unwrap_or(0);
                let entries = match args.get("BrowseFlag") {
                    "BrowseMetadata" => match self.metadata(&node) {
                        Some(e) => vec![e],
                        None => return Reply::Err(701, "No such object"),
                    },
                    "BrowseDirectChildren" => {
                        if matches!(node, Node::Track { .. } | Node::PlaylistItem { .. }) {
                            return Reply::Err(710, "No such container");
                        }
                        match self.children(&node) {
                            Some(c) => c,
                            None => return Reply::Err(701, "No such object"),
                        }
                    }
                    _ => return Reply::Err(402, "Invalid Args"),
                };
                let (result, n, total) = page(&entries, start, count, base);
                ok(&[
                    ("Result", &result),
                    ("NumberReturned", &n.to_string()),
                    ("TotalMatches", &total.to_string()),
                    ("UpdateID", &update_id),
                ])
            }
            "Search" => {
                let Some(expr) = parse_criteria(args.get("SearchCriteria")) else {
                    return Reply::Err(708, "Unsupported or invalid search criteria");
                };
                let entries = self.search(&expr);
                let start = args.num::<usize>("StartingIndex").unwrap_or(0);
                let count = args.num::<usize>("RequestCount").unwrap_or(0);
                let (result, n, total) = page(&entries, start, count, base);
                ok(&[
                    ("Result", &result),
                    ("NumberReturned", &n.to_string()),
                    ("TotalMatches", &total.to_string()),
                    ("UpdateID", &update_id),
                ])
            }
            _ => Reply::Err(401, "Invalid Action"),
        }
    }

    fn search(&self, expr: &Expr) -> Vec<Entry> {
        let lib = &self.ctl.lib;
        let mut values = Vec::new();
        expr.text_values(&mut values);
        let (mut tracks, mut albums, mut artists) = (Vec::new(), Vec::new(), Vec::new());
        if values.is_empty() {
            tracks = lib.tracks(TrackSort::Artist);
            albums = lib.albums(AlbumSort::Title);
            artists = lib.artists();
        } else {
            for v in values {
                let r = lib.search(&v);
                for t in r.tracks {
                    if !tracks.iter().any(|x: &Track| x.path == t.path) {
                        tracks.push(t);
                    }
                }
                for a in r.albums {
                    if !albums.iter().any(|x: &Album| x.id == a.id) {
                        albums.push(a);
                    }
                }
                for a in r.artists {
                    if !artists.iter().any(|x: &Artist| x.name == a.name) {
                        artists.push(a);
                    }
                }
            }
        }
        let mut out: Vec<Entry> = Vec::new();
        out.extend(
            artists
                .into_iter()
                .filter(|a| expr.eval(&Obj::Artist(a)))
                .map(Entry::Artist),
        );
        out.extend(
            albums
                .into_iter()
                .filter(|a| expr.eval(&Obj::Album(a)))
                .map(|album| Entry::Album {
                    parent: "albums".into(),
                    album,
                }),
        );
        out.extend(
            tracks
                .into_iter()
                .filter(|t| expr.eval(&Obj::Track(t)))
                .map(|track| Entry::Track {
                    parent: "tracks".into(),
                    track,
                }),
        );
        out
    }
}

fn page(entries: &[Entry], start: usize, count: usize, base: &str) -> (String, usize, usize) {
    let total = entries.len();
    let end = if count == 0 {
        total
    } else {
        start.saturating_add(count).min(total)
    };
    let slice = entries.get(start.min(total)..end).unwrap_or(&[]);
    let mut out = String::from(didl::DIDL_OPEN);
    for e in slice {
        out.push_str(&e.render(base));
    }
    out.push_str(didl::DIDL_CLOSE);
    (out, slice.len(), total)
}

// ------------------------------------------------------------- search

/// What a criteria is evaluated against.
enum Obj<'a> {
    Track(&'a Track),
    Album(&'a Album),
    Artist(&'a Artist),
}

impl Obj<'_> {
    fn class(&self) -> &'static str {
        match self {
            Obj::Track(_) => "object.item.audioItem.musicTrack",
            Obj::Album(_) => "object.container.album.musicAlbum",
            Obj::Artist(_) => "object.container.person.musicArtist",
        }
    }

    fn prop(&self, name: &str) -> Vec<String> {
        let o = |v: &Option<String>| v.iter().cloned().collect::<Vec<_>>();
        match (self, name) {
            (_, "upnp:class") => vec![self.class().to_string()],
            (Obj::Track(t), "dc:title") => vec![t.title.clone()],
            (Obj::Track(t), "upnp:artist" | "dc:creator") => {
                let mut v = o(&t.artist);
                v.extend(o(&t.album_artist));
                v
            }
            (Obj::Track(t), "upnp:album") => o(&t.album),
            (Obj::Track(t), "upnp:genre") => o(&t.genre),
            (Obj::Album(a), "dc:title" | "upnp:album") => vec![a.title.clone()],
            (Obj::Album(a), "upnp:artist" | "dc:creator") => vec![a.artist.clone()],
            (Obj::Album(a), "upnp:genre") => o(&a.genre),
            (Obj::Artist(a), "dc:title" | "upnp:artist" | "dc:creator") => vec![a.name.clone()],
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Expr {
    All,
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Rel(String, String, String),
}

impl Expr {
    fn eval(&self, o: &Obj<'_>) -> bool {
        match self {
            Expr::All => true,
            Expr::And(a, b) => a.eval(o) && b.eval(o),
            Expr::Or(a, b) => a.eval(o) || b.eval(o),
            Expr::Rel(prop, op, val) => {
                let vals = o.prop(prop);
                let v = val.to_lowercase();
                match op.as_str() {
                    "exists" => (!vals.is_empty()) == (v == "true"),
                    "derivedfrom" => vals.iter().any(|x| x.to_lowercase().starts_with(&v)),
                    "=" => vals.iter().any(|x| x.to_lowercase() == v),
                    "!=" => vals.iter().all(|x| x.to_lowercase() != v),
                    "contains" => vals.iter().any(|x| x.to_lowercase().contains(&v)),
                    "doesnotcontain" => vals.iter().all(|x| !x.to_lowercase().contains(&v)),
                    _ => false,
                }
            }
        }
    }

    /// Free-text values that can seed a library search.
    fn text_values(&self, out: &mut Vec<String>) {
        match self {
            Expr::All => {}
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.text_values(out);
                b.text_values(out);
            }
            Expr::Rel(prop, op, val) => {
                if prop != "upnp:class" && (op == "contains" || op == "=") && !val.is_empty() {
                    out.push(val.clone());
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
enum Tok {
    Open,
    Close,
    Str(String),
    Word(String),
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(&c) = it.peek() {
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '(' => {
                it.next();
                out.push(Tok::Open);
            }
            ')' => {
                it.next();
                out.push(Tok::Close);
            }
            '"' => {
                it.next();
                let mut v = String::new();
                loop {
                    match it.next()? {
                        '\\' => v.push(it.next()?),
                        '"' => break,
                        c => v.push(c),
                    }
                }
                out.push(Tok::Str(v));
            }
            '=' | '!' | '<' | '>' => {
                let mut v = String::new();
                while let Some(&c) = it.peek() {
                    if matches!(c, '=' | '!' | '<' | '>') {
                        v.push(c);
                        it.next();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Word(v));
            }
            _ => {
                let mut v = String::new();
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() || matches!(c, '(' | ')' | '"' | '=' | '!' | '<' | '>') {
                        break;
                    }
                    v.push(c);
                    it.next();
                }
                out.push(Tok::Word(v));
            }
        }
    }
    Some(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek_word(&self, w: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn or(&mut self) -> Option<Expr> {
        let mut e = self.and()?;
        while self.peek_word("or") {
            self.pos += 1;
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Some(e)
    }

    fn and(&mut self) -> Option<Expr> {
        let mut e = self.factor()?;
        while self.peek_word("and") {
            self.pos += 1;
            e = Expr::And(Box::new(e), Box::new(self.factor()?));
        }
        Some(e)
    }

    fn factor(&mut self) -> Option<Expr> {
        match self.toks.get(self.pos)?.clone() {
            Tok::Open => {
                self.pos += 1;
                let e = self.or()?;
                (self.toks.get(self.pos) == Some(&Tok::Close)).then_some(())?;
                self.pos += 1;
                Some(e)
            }
            Tok::Word(w) if w == "*" => {
                self.pos += 1;
                Some(Expr::All)
            }
            Tok::Word(prop) => {
                let Some(Tok::Word(op)) = self.toks.get(self.pos + 1).cloned() else {
                    return None;
                };
                let val = match self.toks.get(self.pos + 2)? {
                    Tok::Str(s) | Tok::Word(s) => s.clone(),
                    _ => return None,
                };
                self.pos += 3;
                Some(Expr::Rel(prop, op.to_ascii_lowercase(), val))
            }
            _ => None,
        }
    }
}

fn parse_criteria(s: &str) -> Option<Expr> {
    let s = s.trim();
    if s.is_empty() || s == "*" {
        return Some(Expr::All);
    }
    let mut p = Parser {
        toks: tokenize(s)?,
        pos: 0,
    };
    let e = p.or()?;
    (p.pos == p.toks.len()).then_some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        let aid = "ab/c d";
        let id = id_of(&["artists", "AC/DC", aid]);
        assert_eq!(
            parse_id(&id),
            Some(Node::Album {
                parent: id_of(&["artists", "AC/DC"]),
                aid: aid.into()
            })
        );
        let tid = format!("{id}/{}", pct_encode("/music/x.flac"));
        assert_eq!(
            parse_id(&tid),
            Some(Node::Track {
                parent: id,
                path: "/music/x.flac".into()
            })
        );
        assert_eq!(parse_id("0"), Some(Node::Root));
        assert_eq!(parse_id("recent"), Some(Node::Top("recent")));
        assert_eq!(parse_id("nope"), None);
        assert_eq!(parse_id("tracks/a/b"), None);
    }

    #[test]
    fn criteria() {
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and (dc:title contains "tone" or upnp:artist contains "x\"y")"#,
        )
        .unwrap();
        let mut v = Vec::new();
        e.text_values(&mut v);
        assert_eq!(v, ["tone", "x\"y"]);
        assert_eq!(parse_criteria("*"), Some(Expr::All));
        assert!(parse_criteria("dc:title contains").is_none());
        assert!(parse_criteria("(dc:title = \"a\"").is_none());
        let t = Track {
            path: "/p.flac".into(),
            uri: "file:///p.flac".into(),
            title: "A Tone".into(),
            artist: Some("Me".into()),
            ..Default::default()
        };
        assert!(e.eval(&Obj::Track(&t)));
        let c = parse_criteria(r#"upnp:class = "object.container.album.musicAlbum""#).unwrap();
        assert!(!c.eval(&Obj::Track(&t)));
    }
}
