use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::library::Library;

/// Watch library roots; on any change, rescan that root (debounced).
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
                let _ = w.watch(r, RecursiveMode::Recursive);
            }
        }
        let stop_t = stop.clone();
        std::thread::Builder::new()
            .name("ricercar-watch".into())
            .spawn(move || {
                let _watcher = watcher;
                let mut dirty: Option<PathBuf> = None;
                loop {
                    if stop_t.load(Ordering::Relaxed) {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(Ok(ev)) => {
                            if matches!(
                                ev.kind,
                                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
                            ) {
                                dirty = ev.paths.first().cloned().or(dirty);
                            }
                        }
                        Ok(Err(_)) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => break,
                    }
                    if dirty.is_some() {
                        // settle, then rescan all roots (cheap for libraries)
                        std::thread::sleep(Duration::from_millis(800));
                        while rx.try_recv().is_ok() {}
                        dirty = None;
                        for r in roots.iter() {
                            lib.scan_root(r);
                        }
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

/// Scan a root right now (blocking).
pub fn scan(lib: &Arc<Library>, root: &Path) -> usize {
    lib.scan_root(root)
}
