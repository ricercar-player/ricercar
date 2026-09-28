//! Deterministic synthetic libraries for load tests and benchmarks
//! (`examples/synth_library.rs`, `tests/perf.rs`). Not part of the stable API.

use std::path::{Path, PathBuf};

use crate::library::Library;
use crate::meta::TagInfo;

/// splitmix64: small, fast and reproducible across platforms.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, words: &[&'a str]) -> &'a str {
        words[self.below(words.len())]
    }
}

const FIRST: &[&str] = &[
    "Anna", "Bruno", "Clara", "David", "Elena", "Felix", "Greta", "Hugo", "Ines", "Jonas", "Karin",
    "Lucas", "Maria", "Nils", "Olga", "Pablo", "Quentin", "Rosa", "Sven", "Tomas", "Ulla",
    "Victor", "Wanda", "Yann", "Zoé", "Björn", "Chloé", "Émile",
];
const LAST: &[&str] = &[
    "Moreau",
    "Lindqvist",
    "Okafor",
    "Brandt",
    "Sato",
    "Ferreira",
    "Novak",
    "Haddad",
    "Kowalski",
    "Rossi",
    "Dupont",
    "Nakamura",
    "Jensen",
    "Alvarez",
    "Petrov",
    "Keller",
    "Marsh",
    "Ortega",
    "Laurent",
    "Weber",
    "Castillo",
    "Byrne",
    "Fischer",
    "Varga",
];
const ENSEMBLE: &[&str] = &[
    "Quartet",
    "Trio",
    "Ensemble",
    "Orchestra",
    "Collective",
    "Sextet",
    "Consort",
    "Band",
];
const ADJ: &[&str] = &[
    "Blue", "Silent", "Golden", "Distant", "Northern", "Electric", "Quiet", "Velvet", "Broken",
    "Endless", "Hidden", "Crimson", "Paper", "Winter", "Hollow", "Bright", "Lunar", "Wild", "Slow",
    "Amber", "Open", "Last", "Early", "Deep",
];
const NOUN: &[&str] = &[
    "Rain", "Harbor", "Lights", "Garden", "River", "Fugue", "Mirror", "Horizon", "Engine", "Tide",
    "Signal", "Forest", "Canyon", "Letters", "Stones", "Bridge", "Motion", "Glass", "Echo", "Room",
    "Station", "Season", "Machine", "Waltz",
];
const GENRES: &[&str] = &[
    "Jazz",
    "Classical",
    "Rock",
    "Electronic",
    "Ambient",
    "Folk",
    "Soul",
    "Hip-Hop",
    "Blues",
    "Pop",
    "Metal",
    "Reggae",
    "Baroque",
    "Chamber Music",
    "Opera",
    "Funk",
    "House",
    "Techno",
    "Country",
    "Latin",
    "World",
    "Soundtrack",
    "Indie",
    "Punk",
];

/// (codec, sample rate, bits, kbit/s, weight): mostly CD quality, a real
/// share of hi-res, a few lossy files and very high rates.
const FORMATS: &[(&str, u32, Option<u8>, u32, usize)] = &[
    ("FLAC", 44_100, Some(16), 900, 60),
    ("MP3", 44_100, None, 320, 6),
    ("FLAC", 48_000, Some(24), 1_700, 4),
    ("FLAC", 88_200, Some(24), 2_900, 5),
    ("FLAC", 96_000, Some(24), 3_100, 14),
    ("FLAC", 192_000, Some(24), 5_800, 8),
    ("FLAC", 352_800, Some(24), 9_200, 3),
];

fn format(rng: &mut Rng) -> (&'static str, u32, Option<u8>, u32) {
    let total: usize = FORMATS.iter().map(|f| f.4).sum();
    let mut n = rng.below(total);
    for &(c, r, b, k, w) in FORMATS {
        if n < w {
            return (c, r, b, k);
        }
        n -= w;
    }
    unreachable!()
}

fn safe(s: &str) -> String {
    s.chars().map(|c| if c == '/' { '-' } else { c }).collect()
}

/// One album of a synthetic library.
pub struct SynthAlbum {
    pub dir: PathBuf,
    /// Tracks: path, tags and a plausible file size in bytes.
    pub tracks: Vec<(PathBuf, TagInfo, i64)>,
    /// Cover colour.
    pub rgb: [u8; 3],
}

/// About `tracks` tracks under `root`: ~12 tracks per album, ~5 albums per
/// artist, some compilations and multi-disc albums. Same input, same output.
pub fn albums(root: &Path, tracks: usize) -> Vec<SynthAlbum> {
    let mut rng = Rng(0x005e_ed0f_71ce_2026);
    let artists: Vec<String> = (0..(tracks / 62).max(1))
        .map(|i| match i % 4 {
            3 => format!("{} {}", rng.pick(LAST), rng.pick(ENSEMBLE)),
            _ => format!("{} {}", rng.pick(FIRST), rng.pick(LAST)),
        })
        .collect();
    let mut out = Vec::new();
    let mut made = 0;
    let mut n = 0;
    while made < tracks {
        let compilation = n % 20 == 7;
        let artist = if compilation {
            "Various Artists".to_string()
        } else {
            artists[rng.below(artists.len())].clone()
        };
        let title = format!("{} {}", rng.pick(ADJ), rng.pick(NOUN));
        let title = if n % 9 == 4 {
            format!("{title} (Deluxe Edition)")
        } else {
            title
        };
        let year = 1955 + rng.below(70) as i32;
        let genre = rng.pick(GENRES).to_string();
        let (codec, rate, bits, kbps) = format(&mut rng);
        let discs = if n % 10 == 3 { 2 } else { 1 };
        let count = (8 + rng.below(9)).min(tracks - made);
        let dir = root
            .join(safe(&artist))
            .join(format!("{year} - {} [{n}]", safe(&title)));
        let mut list = Vec::with_capacity(count);
        for i in 0..count {
            let disc = if discs > 1 { 1 + (i * 2 / count) } else { 1 };
            let no = (i + 1) as u32;
            let track_title = format!("{} {}", rng.pick(ADJ), rng.pick(NOUN));
            let track_artist = if compilation {
                artists[rng.below(artists.len())].clone()
            } else {
                artist.clone()
            };
            let duration_ms = 120_000 + rng.below(360_000) as u64;
            let ext = if codec == "MP3" { "mp3" } else { "flac" };
            let path = dir.join(format!("{disc}-{no:02} {}.{ext}", safe(&track_title)));
            let tags = TagInfo {
                title: Some(track_title),
                artist: Some(track_artist),
                album_artist: Some(artist.clone()),
                album: Some(title.clone()),
                track: Some(no),
                disc: Some(disc as u32),
                year: Some(year),
                genre: Some(genre.clone()),
                duration_ms,
                sample_rate: Some(rate),
                bits,
                channels: Some(2),
                bitrate: Some(kbps),
                codec: Some(codec.to_string()),
                ..Default::default()
            };
            let size = (duration_ms as i64) * kbps as i64 / 8;
            list.push((path, tags, size));
        }
        made += count;
        let rgb = [
            40 + rng.below(180) as u8,
            40 + rng.below(180) as u8,
            40 + rng.below(180) as u8,
        ];
        out.push(SynthAlbum {
            dir,
            tracks: list,
            rgb,
        });
        n += 1;
    }
    out
}

/// Insert a synthetic library straight into the database (no files).
pub fn fill(lib: &Library, albums: &[SynthAlbum]) -> usize {
    let mut added = 0;
    for chunk in albums.chunks(200) {
        let items: Vec<_> = chunk
            .iter()
            .flat_map(|a| a.tracks.iter())
            .map(|(p, t, size)| (p.clone(), t.clone(), *size, 1))
            .collect();
        added += lib.upsert_many(&items);
    }
    added
}

/// A small `cover.jpg` in the album folder.
pub fn write_cover(album: &SynthAlbum) -> std::io::Result<()> {
    std::fs::create_dir_all(&album.dir)?;
    let [r, g, b] = album.rgb;
    let img = image::RgbImage::from_fn(96, 96, |x, y| {
        let shade = |c: u8| c.saturating_sub(((x + y) / 4) as u8);
        image::Rgb([shade(r), shade(g), shade(b)])
    });
    img.save(album.dir.join("cover.jpg"))
        .map_err(std::io::Error::other)
}

// ---------------------------------------------------------------- FLAC

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new() -> Bits {
        Bits {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }

    fn put(&mut self, value: u64, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A valid, very short FLAC file of digital silence (one second, constant
/// subframes) carrying the given tags.
pub fn write_flac(path: &Path, tags: &TagInfo) -> std::io::Result<()> {
    const BLOCK: u64 = 4096;
    let rate = tags.sample_rate.unwrap_or(44_100) as u64;
    let bps = tags.bits.unwrap_or(16) as u64;
    let frames = rate.div_ceil(BLOCK);
    let mut f = b"fLaC".to_vec();

    // STREAMINFO
    let mut si = Bits::new();
    si.put(BLOCK, 16);
    si.put(BLOCK, 16);
    si.put(0, 24);
    si.put(0, 24);
    si.put(rate, 20);
    si.put(1, 3); // 2 channels
    si.put(bps - 1, 5);
    si.put(frames * BLOCK, 36);
    si.put(0, 64);
    si.put(0, 64); // no MD5
    f.extend_from_slice(&[0x00, 0, 0, 34]);
    f.extend_from_slice(&si.out);

    // VORBIS_COMMENT (last metadata block)
    let mut comments = Vec::new();
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            comments.push(format!("{k}={v}"));
        }
    };
    add("TITLE", tags.title.clone());
    add("ARTIST", tags.artist.clone());
    add("ALBUMARTIST", tags.album_artist.clone());
    add("ALBUM", tags.album.clone());
    add("TRACKNUMBER", tags.track.map(|n| n.to_string()));
    add("DISCNUMBER", tags.disc.map(|n| n.to_string()));
    add("DATE", tags.year.map(|n| n.to_string()));
    add("GENRE", tags.genre.clone());
    let vendor = b"ricercar synth";
    let mut vc = Vec::new();
    vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendor);
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in &comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    let len = vc.len() as u32;
    f.extend_from_slice(&[0x84, (len >> 16) as u8, (len >> 8) as u8, len as u8]);
    f.extend_from_slice(&vc);

    // Frames: fixed 4096-sample blocks, rate and depth from STREAMINFO.
    for i in 0..frames {
        let mut h = vec![0xff, 0xf8, 0b1100_0000, 0b0001_0000];
        // Frame number, UTF-8 style.
        if i < 0x80 {
            h.push(i as u8);
        } else {
            h.push(0xc0 | (i >> 6) as u8);
            h.push(0x80 | (i & 0x3f) as u8);
        }
        h.push(crc8(&h));
        for _ in 0..2 {
            h.push(0); // CONSTANT subframe
            h.extend(std::iter::repeat_n(0u8, (bps / 8) as usize));
        }
        let crc = crc16(&h);
        h.extend_from_slice(&crc.to_be_bytes());
        f.extend_from_slice(&h);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sized() {
        let a = albums(Path::new("/s"), 1000);
        let b = albums(Path::new("/s"), 1000);
        let n: usize = a.iter().map(|x| x.tracks.len()).sum();
        assert_eq!(n, 1000);
        assert_eq!(a[3].tracks[0].0, b[3].tracks[0].0);
        assert!(a.len() > 50 && a.len() < 130);
    }

    #[test]
    fn flac_files_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let al = &albums(dir.path(), 40)[0];
        let (path, tags, _) = &al.tracks[0];
        let path = path.with_extension("flac");
        let mut tags = tags.clone();
        tags.sample_rate = Some(96_000);
        tags.bits = Some(24);
        write_flac(&path, &tags).unwrap();
        write_cover(al).unwrap();
        let read = crate::meta::read_tags(&path);
        assert_eq!(read.title, tags.title);
        assert_eq!(read.album_artist, tags.album_artist);
        assert_eq!(read.sample_rate, Some(96_000));
        assert_eq!(read.bits, Some(24));
        assert_eq!(read.codec.as_deref(), Some("FLAC"));
        assert!(crate::meta::folder_cover(&path).is_some());

        let lib = Library::in_memory().unwrap();
        assert_eq!(fill(&lib, &albums(Path::new("/s"), 300)), 300);
        assert_eq!(lib.count(), 300);
    }
}
