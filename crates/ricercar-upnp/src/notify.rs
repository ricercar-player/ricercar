//! The event thread: turns Controller events into GENA notifications.
//!
//! Each wake-up recomputes the evented variables of the affected services
//! from one state snapshot, diffs them against what was last sent and
//! notifies only what changed (OpenHome: individual variables; UPnP AV:
//! a `LastChange` document). New subscribers get the full state (SEQ 0).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ricercar_core::CtlEvent;

use crate::events::{last_change, propertyset};
use crate::{Renderer, Snap, Svc, Wake};

pub type Vars = Vec<(&'static str, String)>;

const AVT_META_NS: &str = "urn:schemas-upnp-org:metadata-1-0/AVT/";
const RCS_META_NS: &str = "urn:schemas-upnp-org:metadata-1-0/RCS/";
const TICK: Duration = Duration::from_secs(1);

impl Renderer {
    /// Every evented variable of a service, from one snapshot.
    pub fn vars(&self, svc: Svc, s: &Snap) -> Vars {
        let base = format!("http://{}", self.default_host);
        match svc {
            Svc::Avt => self.avt_vars(s, &base),
            Svc::Rcs => vec![
                ("Volume", s.volume.to_string()),
                ("Mute", if s.muted { "1" } else { "0" }.into()),
            ],
            Svc::Cms => crate::avt::cms_vars(false),
            Svc::ServerCms => crate::avt::cms_vars(true),
            Svc::Cd => vec![("SystemUpdateID", self.system_update_id().to_string())],
            Svc::OhProduct => self.oh_product_vars(),
            Svc::OhPlaylist => self.oh_playlist_vars(s),
            Svc::OhInfo => self.oh_info_vars(s, &base),
            Svc::OhTime => self.oh_time_vars(s),
            Svc::OhVolume => self.oh_volume_vars(s),
        }
    }

    /// NOTIFY body for a set of variables of a service.
    pub fn event_body(&self, svc: Svc, vars: &Vars) -> String {
        match svc {
            Svc::Avt => propertyset(&[("LastChange", last_change(AVT_META_NS, vars, false))]),
            Svc::Rcs => propertyset(&[("LastChange", last_change(RCS_META_NS, vars, true))]),
            _ => propertyset(vars),
        }
    }
}

/// Services whose variables may change on a Controller event.
pub fn affected(ev: &CtlEvent) -> &'static [Svc] {
    match ev {
        CtlEvent::VolumeChanged => &[Svc::Rcs, Svc::OhVolume],
        CtlEvent::Seeked(_) => &[Svc::OhTime],
        CtlEvent::StreamTitle(_) => &[Svc::OhInfo],
        CtlEvent::Played(_) | CtlEvent::Error(_) => &[],
        CtlEvent::TrackChanged | CtlEvent::StatusChanged(_) | CtlEvent::QueueChanged => &[
            Svc::Avt,
            Svc::OhPlaylist,
            Svc::OhInfo,
            Svc::OhTime,
            Svc::OhProduct,
        ],
    }
}

pub fn event_loop(r: Arc<Renderer>, rx: Receiver<Wake>, stop: Arc<AtomicBool>) {
    let mut last: HashMap<Svc, Vars> = HashMap::new();
    // Seed with the current state so the first real change is a diff.
    {
        let s = r.snap();
        for svc in Svc::ALL {
            last.insert(svc, r.vars(svc, &s));
        }
    }
    let mut next_tick = Instant::now() + TICK;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        let first = match rx.recv_timeout(wait) {
            Ok(w) => Some(w),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut dirty: Vec<Svc> = Vec::new();
        let mut new_subs: Vec<(Svc, String)> = Vec::new();
        let mut queue_changed = false;
        let mut handle = |w: Wake, dirty: &mut Vec<Svc>| match w {
            Wake::Ctl(svcs, queue) => {
                queue_changed |= queue;
                dirty.extend_from_slice(svcs);
            }
            Wake::Dirty => dirty.extend_from_slice(&Svc::ALL),
            Wake::NewSub(svc, sid) => new_subs.push((svc, sid)),
        };
        if let Some(w) = first {
            handle(w, &mut dirty);
        }
        // Coalesce bursts (a track change emits several events).
        while let Ok(w) = rx.try_recv() {
            handle(w, &mut dirty);
        }
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + TICK;
            for svc in Svc::ALL {
                r.subs(svc).expire();
            }
            dirty.extend_from_slice(&[Svc::OhTime, Svc::Cd]);
        }
        if dirty.is_empty() && new_subs.is_empty() {
            continue;
        }
        let s = r.snap();
        if queue_changed {
            r.prune_meta();
        }
        r.update_counters(&s);
        dirty.sort_by_key(|s| s.path());
        dirty.dedup();
        for svc in dirty {
            let vars = r.vars(svc, &s);
            let prev = last.get(&svc);
            let changed: Vars = vars
                .iter()
                .filter(|(k, v)| {
                    prev.and_then(|p| p.iter().find(|(pk, _)| pk == k))
                        .map(|(_, pv)| pv != v)
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            if !changed.is_empty() && r.subs(svc).count() > 0 {
                r.subs(svc).notify(&r.event_body(svc, &changed));
            }
            last.insert(svc, vars);
        }
        for (svc, sid) in new_subs {
            let vars = r.vars(svc, &s);
            r.subs(svc).initial(&sid, &r.event_body(svc, &vars));
        }
    }
}
