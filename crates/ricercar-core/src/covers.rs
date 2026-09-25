//! Album art: resolve (embedded → folder → downloaded), cache resized JPEG
//! thumbnails, and extract an accent colour for adaptive theming.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image::imageops::FilterType;

use crate::meta;

pub struct CoverCache {
    dir: PathBuf,
    /// Keys known to have no art during this session.
    misses: Mutex<HashSet<String>>,
}

impl CoverCache {
    pub fn new(dir: PathBuf) -> CoverCache {
        let _ = std::fs::create_dir_all(&dir);
        CoverCache {
            dir,
            misses: Mutex::new(HashSet::new()),
        }
    }

    pub fn default_dir() -> PathBuf {
        crate::config::cache_dir().join("covers")
    }

    fn original_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}-orig", sanitize(key)))
    }

    fn thumb_path(&self, key: &str, size: u32) -> PathBuf {
        self.dir.join(format!("{}-{size}.jpg", sanitize(key)))
    }

    /// Store art fetched online (Cover Art Archive…) for a key.
    pub fn put_original(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        crate::config::write_atomic(&self.original_path(key), bytes)?;
        self.misses.lock().unwrap().remove(key);
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{}-", sanitize(key))) && name.ends_with(".jpg") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    pub fn has_art(&self, key: &str, track: &Path) -> bool {
        self.original_path(key).exists()
            || meta::folder_cover(track).is_some()
            || meta::embedded_cover(track).is_some()
    }

    /// Full-resolution art bytes.
    pub fn source(&self, key: &str, track: &Path) -> Option<Vec<u8>> {
        meta::cover_bytes(track)
            .map(|(b, _)| b)
            .or_else(|| std::fs::read(self.original_path(key)).ok())
    }

    /// A JPEG no larger than `size`×`size`, generated on first use.
    pub fn thumb(&self, key: &str, track: &Path, size: u32) -> Option<PathBuf> {
        let out = self.thumb_path(key, size);
        if out.exists() {
            return Some(out);
        }
        if self.misses.lock().unwrap().contains(key) {
            return None;
        }
        let Some(bytes) = self.source(key, track) else {
            self.misses.lock().unwrap().insert(key.to_string());
            return None;
        };
        let img = match image::load_from_memory(&bytes) {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!("cover {}: {e}", track.display());
                self.misses.lock().unwrap().insert(key.to_string());
                return None;
            }
        };
        let img = if img.width() > size || img.height() > size {
            img.resize(size, size, FilterType::CatmullRom)
        } else {
            img
        };
        let mut buf = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
        img.to_rgb8().write_with_encoder(enc).ok()?;
        crate::config::write_atomic(&out, &buf).ok()?;
        Some(out)
    }

    pub fn forget_misses(&self) {
        self.misses.lock().unwrap().clear();
    }
}

fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// A saturated, readable accent colour from an image (r, g, b).
pub fn accent_color(path: &Path) -> Option<[u8; 3]> {
    let img = image::open(path).ok()?.thumbnail(48, 48).to_rgb8();
    let mut bins = [[0f64; 4]; 12]; // weight, r, g, b
    for p in img.pixels() {
        let [r, g, b] = p.0.map(|c| c as f64 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        if max < 0.15 || delta < 0.08 {
            continue;
        }
        let s = delta / max;
        let h = if max == r {
            ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        } * 60.0;
        let w = s * s * max;
        let bin = &mut bins[((h / 30.0) as usize).min(11)];
        bin[0] += w;
        bin[1] += r * w;
        bin[2] += g * w;
        bin[3] += b * w;
    }
    let best = bins.iter().max_by(|a, b| a[0].total_cmp(&b[0]))?;
    if best[0] < 0.5 {
        return None;
    }
    let mut rgb = [best[1] / best[0], best[2] / best[0], best[3] / best[0]];
    // keep it legible on a dark background: lift very dark accents
    let max = rgb.iter().cloned().fold(0.0, f64::max);
    if max < 0.6 {
        let k = 0.6 / max.max(0.01);
        rgb = rgb.map(|c| (c * k).min(1.0));
    }
    Some(rgb.map(|c| (c * 255.0).round() as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(dir: &Path, name: &str, rgb: [u8; 3]) -> PathBuf {
        let p = dir.join(name);
        image::RgbImage::from_pixel(600, 600, image::Rgb(rgb))
            .save(&p)
            .unwrap();
        p
    }

    #[test]
    fn folder_art_thumbnails_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        let album = dir.path().join("album");
        std::fs::create_dir_all(&album).unwrap();
        let track = album.join("01.flac");
        std::fs::write(&track, b"x").unwrap();
        let cache = CoverCache::new(dir.path().join("cache"));
        assert!(cache.thumb("k", &track, 200).is_none());
        png(&album, "cover.png", [200, 30, 30]);
        cache.forget_misses();
        let t = cache.thumb("k", &track, 200).unwrap();
        let img = image::open(&t).unwrap();
        assert_eq!((img.width(), img.height()), (200, 200));
        let c = accent_color(&t).unwrap();
        assert!(c[0] > 150 && c[1] < 80, "{c:?}");
    }

    #[test]
    fn online_original_is_used_and_invalidates_thumbs() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("t.flac");
        std::fs::write(&track, b"x").unwrap();
        let cache = CoverCache::new(dir.path().join("cache"));
        let src = png(dir.path(), "dl.png", [10, 10, 220]);
        cache
            .put_original("alb", &std::fs::read(src).unwrap())
            .unwrap();
        assert!(cache.has_art("alb", &track));
        assert!(cache.thumb("alb", &track, 64).is_some());
    }

    #[test]
    fn grey_images_have_no_accent() {
        let dir = tempfile::tempdir().unwrap();
        let p = png(dir.path(), "g.png", [128, 128, 128]);
        assert!(accent_color(&p).is_none());
    }
}
