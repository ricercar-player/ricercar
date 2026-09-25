//! ListenBrainz / Last.fm scrobbling driven by controller events. Each service
//! has its own persistent queue so an outage of one never loses listens.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ricercar_core::config::{self, Config};
use ricercar_core::{Controller, CtlEvent, TrackInfo};
use ricercar_online::lastfm::LastFm;
use ricercar_online::listenbrainz::ListenBrainz;
use ricercar_online::scrobble::{Scrobble, ScrobbleQueue, ScrobbleTrack};

fn now_unix() -> i64 {
    ricercar_core::library::now_unix()
}

fn to_track(info: &TrackInfo) -> Option<ScrobbleTrack> {
    let artist = info.artist.clone().filter(|a| !a.is_empty())?;
    if info.live || info.title.is_empty() {
        return None;
    }
    Some(ScrobbleTrack {
        artist,
        title: info.title.clone(),
        album: info.album.clone(),
        album_artist: info.album_artist.clone(),
        duration_ms: info.duration_ms,
        track_number: info.track_no,
        mbid: None,
    })
}

struct Services {
    lb: Option<ListenBrainz>,
    lastfm: Option<(LastFm, String)>,
}

fn services(cfg: &Config) -> Services {
    let s = &cfg.scrobble;
    Services {
        lb: (!s.listenbrainz_token.is_empty())
            .then(|| ListenBrainz::new(s.listenbrainz_token.clone())),
        lastfm: (!s.lastfm_session.is_empty() && !s.lastfm_api_key.is_empty()).then(|| {
            (
                LastFm::new(s.lastfm_api_key.clone(), s.lastfm_secret.clone()),
                s.lastfm_session.clone(),
            )
        }),
    }
}

fn flush(svc: &Services, lb_q: &mut ScrobbleQueue, lf_q: &mut ScrobbleQueue) {
    if let Some(lb) = &svc.lb {
        while let Ok(batch) = lb_q.drain_batch(100) {
            if batch.is_empty() {
                break;
            }
            if let Err(e) = lb.submit(&batch) {
                tracing::warn!("ListenBrainz: {e}");
                if e.is_transient() {
                    let _ = lb_q.requeue(batch);
                }
                break;
            }
        }
    }
    if let Some((lf, sk)) = &svc.lastfm {
        while let Ok(batch) = lf_q.drain_batch(50) {
            if batch.is_empty() {
                break;
            }
            if let Err(e) = lf.scrobble(sk, &batch) {
                tracing::warn!("Last.fm: {e}");
                if e.is_transient() {
                    let _ = lf_q.requeue(batch);
                }
                break;
            }
        }
    }
}

pub fn spawn(ctl: Arc<Controller>, config: Arc<RwLock<Config>>) {
    let events = ctl.subscribe();
    std::thread::Builder::new()
        .name("ricercar-scrobble".into())
        .spawn(move || {
            let dir = config::data_dir();
            let (Ok(mut lb_q), Ok(mut lf_q)) = (
                ScrobbleQueue::open(dir.join("scrobbles-listenbrainz.json")),
                ScrobbleQueue::open(dir.join("scrobbles-lastfm.json")),
            ) else {
                tracing::warn!("scrobble queues unavailable");
                return;
            };
            let mut started: Option<(String, i64)> = None;
            let mut last_retry = Instant::now();
            loop {
                let ev = match events.recv_timeout(Duration::from_secs(60)) {
                    Ok(ev) => Some(ev),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(_) => break,
                };
                let svc = services(&config.read().unwrap());
                if svc.lb.is_none() && svc.lastfm.is_none() {
                    continue;
                }
                match ev {
                    Some(CtlEvent::TrackChanged) => {
                        let info = ctl.lock().track();
                        let Some(info) = info else { continue };
                        if started.as_ref().map(|s| &s.0) == Some(&info.uri) {
                            continue;
                        }
                        started = Some((info.uri.clone(), now_unix()));
                        if let Some(track) = to_track(&info) {
                            if let Some(lb) = &svc.lb
                                && let Err(e) = lb.playing_now(&track)
                            {
                                tracing::debug!("ListenBrainz now playing: {e}");
                            }
                            if let Some((lf, sk)) = &svc.lastfm
                                && let Err(e) = lf.update_now_playing(sk, &track)
                            {
                                tracing::debug!("Last.fm now playing: {e}");
                            }
                        }
                    }
                    Some(CtlEvent::Played(info)) => {
                        let Some(track) = to_track(&info) else {
                            continue;
                        };
                        let at = started
                            .as_ref()
                            .filter(|s| s.0 == info.uri)
                            .map(|s| s.1)
                            .unwrap_or_else(|| now_unix() - (info.duration_ms / 2000) as i64);
                        let s = Scrobble {
                            track,
                            started_at_unix: at,
                        };
                        if svc.lb.is_some() {
                            let _ = lb_q.push(s.clone());
                        }
                        if svc.lastfm.is_some() {
                            let _ = lf_q.push(s);
                        }
                        flush(&svc, &mut lb_q, &mut lf_q);
                        last_retry = Instant::now();
                    }
                    _ => {
                        if (!lb_q.is_empty() || !lf_q.is_empty())
                            && last_retry.elapsed() > Duration::from_secs(300)
                        {
                            flush(&svc, &mut lb_q, &mut lf_q);
                            last_retry = Instant::now();
                        }
                    }
                }
            }
        })
        .expect("spawn scrobbler");
}
