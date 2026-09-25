//! Background cover loading: thumbnails are produced (or fetched) and
//! decoded on worker threads, then handed to the UI thread as pixel buffers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use image::imageops::FilterType;
use ricercar_core::covers::CoverCache;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

#[derive(Clone, Debug)]
pub enum Source {
    /// A track file (embedded / folder art), keyed by album id or path.
    Track { key: String, path: PathBuf },
    /// An http(s) image (UPnP album art, radio favicon).
    Url(String),
}

impl Source {
    fn key(&self) -> &str {
        match self {
            Source::Track { key, .. } => key,
            Source::Url(u) => u,
        }
    }
}

/// When local art is missing, what to ask MusicBrainz for.
#[derive(Clone, Debug)]
pub struct Lookup {
    pub artist: String,
    pub album: String,
}

struct Job {
    id: String,
    src: Source,
    size: u32,
    lookup: Option<Lookup>,
}

pub type Delivery = Box<dyn Fn(String, Option<SharedPixelBuffer<Rgba8Pixel>>) + Send + Sync>;

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
    covers: Arc<CoverCache>,
    online: Mutex<bool>,
    tried_online: Mutex<HashSet<String>>,
    deliver: Delivery,
}

/// UI-thread side: cache of decoded images + in-flight bookkeeping.
pub struct Loader {
    shared: Arc<Shared>,
    cache: HashMap<String, (Image, u64)>,
    in_flight: HashSet<String>,
    missing: HashSet<String>,
    tick: u64,
    capacity: usize,
}

pub fn cache_id(key: &str, size: u32) -> String {
    format!("{size}:{key}")
}

impl Loader {
    pub fn new(covers: Arc<CoverCache>, deliver: Delivery) -> Loader {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            covers,
            online: Mutex::new(false),
            tried_online: Mutex::new(HashSet::new()),
            deliver,
        });
        for i in 0..3 {
            let s = shared.clone();
            std::thread::Builder::new()
                .name(format!("ricercar-covers-{i}"))
                .spawn(move || worker(s))
                .expect("spawn cover worker");
        }
        Loader {
            shared,
            cache: HashMap::new(),
            in_flight: HashSet::new(),
            missing: HashSet::new(),
            tick: 0,
            capacity: 900,
        }
    }

    pub fn set_online(&self, on: bool) {
        *self.shared.online.lock().unwrap() = on;
    }

    /// Cached image, or schedule it and return None.
    pub fn get(&mut self, src: &Source, size: u32, lookup: Option<Lookup>) -> Option<Image> {
        let id = cache_id(src.key(), size);
        self.tick += 1;
        if let Some((img, used)) = self.cache.get_mut(&id) {
            *used = self.tick;
            return Some(img.clone());
        }
        if self.missing.contains(&id) || !self.in_flight.insert(id.clone()) {
            return None;
        }
        let mut q = self.shared.queue.lock().unwrap();
        // Most recent requests first: they are what the user looks at.
        q.push_front(Job {
            id,
            src: src.clone(),
            size,
            lookup,
        });
        self.shared.cv.notify_one();
        None
    }

    pub fn peek(&self, key: &str, size: u32) -> Option<Image> {
        self.cache.get(&cache_id(key, size)).map(|(i, _)| i.clone())
    }

    /// Drop queued (not started) jobs, e.g. after navigating away.
    pub fn cancel_pending(&mut self) {
        let mut q = self.shared.queue.lock().unwrap();
        for j in q.drain(..) {
            self.in_flight.remove(&j.id);
        }
    }

    pub fn arrived(&mut self, id: String, buf: Option<SharedPixelBuffer<Rgba8Pixel>>) {
        self.in_flight.remove(&id);
        match buf {
            Some(b) => {
                self.tick += 1;
                self.cache.insert(id, (Image::from_rgba8(b), self.tick));
                if self.cache.len() > self.capacity {
                    let mut ages: Vec<(u64, String)> = self
                        .cache
                        .iter()
                        .map(|(k, (_, t))| (*t, k.clone()))
                        .collect();
                    ages.sort();
                    for (_, k) in ages.into_iter().take(self.capacity / 4) {
                        self.cache.remove(&k);
                    }
                }
            }
            None => {
                self.missing.insert(id);
            }
        }
    }

    /// Forget negative results (after a rescan / settings change).
    pub fn forget_missing(&mut self) {
        self.missing.clear();
        self.shared.covers.forget_misses();
    }
}

fn worker(s: Arc<Shared>) {
    loop {
        let job = {
            let mut q = s.queue.lock().unwrap();
            loop {
                if let Some(j) = q.pop_front() {
                    break j;
                }
                q = s.cv.wait(q).unwrap();
            }
        };
        let buf = produce(&s, &job);
        (s.deliver)(job.id, buf);
    }
}

fn to_buffer(img: image::DynamicImage, size: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let img = if img.width() > size || img.height() > size {
        img.resize_to_fill(size, size, FilterType::CatmullRom)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height())
}

fn produce(s: &Shared, job: &Job) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    match &job.src {
        Source::Track { key, path } => {
            // Thumbnails are cached on disk at a few fixed sizes.
            let disk = if job.size <= 128 {
                128
            } else if job.size <= 320 {
                320
            } else {
                800
            };
            if let Some(p) = s.covers.thumb(key, path, disk) {
                return image::open(p).ok().map(|i| to_buffer(i, job.size));
            }
            let lookup = job.lookup.as_ref()?;
            if !*s.online.lock().unwrap() || !s.tried_online.lock().unwrap().insert(key.clone()) {
                return None;
            }
            let rel =
                ricercar_online::coverart::find_release(&lookup.artist, &lookup.album).ok()??;
            let bytes = ricercar_online::coverart::front_cover(
                &rel,
                ricercar_online::coverart::CoverSize::S1200,
            )
            .ok()??;
            s.covers.put_original(key, &bytes).ok()?;
            tracing::info!(album = %lookup.album, "fetched cover from Cover Art Archive");
            let p = s.covers.thumb(key, path, disk)?;
            image::open(p).ok().map(|i| to_buffer(i, job.size))
        }
        Source::Url(url) => {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return None;
            }
            let resp = ureq::get(url)
                .set("User-Agent", ricercar_online::USER_AGENT)
                .timeout(std::time::Duration::from_secs(8))
                .call()
                .ok()?;
            let mut bytes = Vec::new();
            resp.into_reader()
                .take(8 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .ok()?;
            image::load_from_memory(&bytes)
                .ok()
                .map(|i| to_buffer(i, job.size))
        }
    }
}

/// Large cover, blurred backdrop and accent colour for the playing track.
pub struct NowArt {
    pub large: Option<SharedPixelBuffer<Rgba8Pixel>>,
    pub backdrop: Option<SharedPixelBuffer<Rgba8Pixel>>,
    pub accent: Option<[u8; 3]>,
}

pub fn now_art(covers: &CoverCache, src: &Source) -> NowArt {
    let img = match src {
        Source::Track { key, path } => covers
            .thumb(key, path, 800)
            .and_then(|p| image::open(p).ok()),
        Source::Url(url) => {
            let resp = ureq::get(url)
                .timeout(std::time::Duration::from_secs(8))
                .call()
                .ok();
            resp.and_then(|r| {
                let mut b = Vec::new();
                r.into_reader().take(16 << 20).read_to_end(&mut b).ok()?;
                image::load_from_memory(&b).ok()
            })
        }
    };
    let Some(img) = img else {
        return NowArt {
            large: None,
            backdrop: None,
            accent: None,
        };
    };
    let small = img.resize_to_fill(96, 96, FilterType::Triangle);
    let backdrop = small.blur(9.0).resize_exact(240, 240, FilterType::Triangle);
    let accent = ricercar_core::covers::accent_of(&small);
    NowArt {
        large: Some(to_buffer(img, 800)),
        backdrop: Some(to_buffer(backdrop, 240)),
        accent,
    }
}
