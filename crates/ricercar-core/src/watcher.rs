use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::library::Library;

/// Watch library roots; after changes settle, run an incremental scan
/// (cheap: only modified files are re-tagged).
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    pub fn start(lib: Arc<Library>, roots: Vec<PathBuf>) -> WatcherHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(tx).ok();
        if let Some(w) = watcher.as_mut() {
            for r in roots.iter() {
                if let Err(e) = w.watch(r, RecursiveMode::Recursive) {
                    tracing::warn!("watch {}: {e}", r.display());
                }
            }
        }
        let stop_t = stop.clone();
        std::thread::Builder::new()
            .name("ricercar-watch".into())
            .spawn(move || {
                let _watcher = watcher;
                let mut dirty = false;
                while !stop_t.load(Ordering::Relaxed) {
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(Ok(ev)) => {
                            if matches!(
                                ev.kind,
                                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
                            ) {
                                dirty = true;
                            }
                        }
                        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => break,
                    }
                    if dirty {
                        // Let copies finish before tagging half-written files.
                        std::thread::sleep(Duration::from_secs(2));
                        while rx.try_recv().is_ok() {}
                        dirty = false;
                        let r = lib.scan_roots(&roots);
                        tracing::info!(?r, "library rescan");
                    }
                }
            })
            .expect("spawn watcher");
        WatcherHandle { stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
