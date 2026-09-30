//! GENA eventing: subscriptions, property sets, NOTIFY delivery.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::xml;

/// Consecutive delivery failures after which a subscriber is dropped.
const MAX_FAILURES: u32 = 3;
/// Subscriptions per service (a misbehaving control point can't grow us).
const MAX_SUBS: usize = 32;
/// Callback URLs kept per subscription.
const MAX_CALLBACKS: usize = 4;
/// Events waiting for a subscriber; one that falls further behind is dropped.
const MAX_BACKLOG: usize = 16;
/// Time spent delivering one event to one subscriber, retries included.
const DELIVERY_BUDGET: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
const IO_TIMEOUT: Duration = Duration::from_millis(2000);

/// (SEQ, body) handed to a subscriber's delivery thread.
type Event = (u32, Arc<str>);

#[derive(Debug)]
struct Sub {
    sid: String,
    /// SEQ of the next event.
    seq: u32,
    expires: Instant,
    /// The initial event (SEQ 0) was queued: regular events may follow.
    ready: bool,
    /// Queue of the delivery thread; dropping it ends that thread.
    tx: SyncSender<Event>,
    /// Set by the delivery thread after repeated failures.
    dead: Arc<AtomicBool>,
}

impl Sub {
    fn alive(&self, now: Instant) -> bool {
        self.expires > now && !self.dead.load(Ordering::Relaxed)
    }

    /// Queue an event; `false` when the subscriber can't take it.
    fn push(&mut self, seq: u32, body: &Arc<str>) -> bool {
        match self.tx.try_send((seq, body.clone())) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

/// One GENA subscription list (per evented service).
#[derive(Default)]
pub struct Subscribers {
    subs: Mutex<Vec<Sub>>,
}

static SID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_sid() -> String {
    let n = SID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "uuid:{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (t >> 32) as u32,
        (t >> 16) as u16,
        (t & 0xfff) as u16,
        (std::process::id() & 0xfff) as u16,
        n & 0xffff_ffff_ffff
    )
}

/// Parse a GENA `CALLBACK` header: one or more `<http://…>` URLs. Only
/// URLs pointing back at the subscriber itself (`peer`, by address, with a
/// port) are kept, at most `MAX_CALLBACKS`.
pub fn parse_callbacks(h: &str, peer: IpAddr) -> Vec<String> {
    h.split('<')
        .filter_map(|p| p.split_once('>').map(|(u, _)| u.trim().to_string()))
        .filter(|u| callback_addr(u).is_some_and(|a| a.ip() == peer && a.port() != 0))
        .take(MAX_CALLBACKS)
        .collect()
}

/// Socket address of an `http://<ip>[:port]/…` callback (no host names).
fn callback_addr(url: &str) -> Option<SocketAddr> {
    split_url(url)?.0.parse().ok()
}

impl Subscribers {
    /// New subscription; `None` when the list is full.
    pub fn subscribe(&self, callbacks: Vec<String>, timeout_secs: u64) -> Option<String> {
        let sid = new_sid();
        let mut subs = self.subs.lock().unwrap();
        let now = Instant::now();
        subs.retain(|s| s.alive(now));
        if subs.len() >= MAX_SUBS {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(MAX_BACKLOG);
        let dead = Arc::new(AtomicBool::new(false));
        {
            let (sid, dead) = (sid.clone(), dead.clone());
            std::thread::Builder::new()
                .name("ricercar-upnp-gena".into())
                .stack_size(128 * 1024)
                .spawn(move || delivery_loop(callbacks, sid, rx, dead))
                .ok()?;
        }
        subs.push(Sub {
            sid: sid.clone(),
            seq: 0,
            expires: now + Duration::from_secs(timeout_secs),
            ready: false,
            tx,
            dead,
        });
        Some(sid)
    }

    pub fn renew(&self, sid: &str, timeout_secs: u64) -> bool {
        let mut subs = self.subs.lock().unwrap();
        let now = Instant::now();
        subs.retain(|s| s.alive(now));
        for s in subs.iter_mut() {
            if s.sid == sid {
                s.expires = now + Duration::from_secs(timeout_secs);
                return true;
            }
        }
        false
    }

    pub fn unsubscribe(&self, sid: &str) -> bool {
        let mut subs = self.subs.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| s.sid != sid);
        subs.len() != before
    }

    /// Drop expired and failing subscriptions.
    pub fn expire(&self) {
        let now = Instant::now();
        self.subs.lock().unwrap().retain(|s| s.alive(now));
    }

    /// Drop every subscription (their delivery threads end).
    pub fn clear(&self) {
        self.subs.lock().unwrap().clear();
    }

    pub fn count(&self) -> usize {
        self.subs.lock().unwrap().len()
    }

    /// Queue the initial event (SEQ 0) for one new subscriber.
    pub fn initial(&self, sid: &str, body: &str) {
        let body: Arc<str> = body.into();
        let mut subs = self.subs.lock().unwrap();
        let Some(s) = subs.iter_mut().find(|s| s.sid == sid && !s.ready) else {
            return;
        };
        s.ready = true;
        s.seq = 1;
        if !s.push(0, &body) {
            subs.retain(|s| s.sid != sid);
        }
    }

    /// Queue an event for every ready subscriber. Delivery happens on each
    /// subscriber's own thread, so a slow or dead one delays nobody else;
    /// one whose backlog is full is dropped.
    pub fn notify(&self, body: &str) {
        let body: Arc<str> = body.into();
        let now = Instant::now();
        let mut subs = self.subs.lock().unwrap();
        subs.retain_mut(|s| {
            if !s.alive(now) {
                return false;
            }
            if !s.ready {
                return true;
            }
            let seq = s.seq;
            // SEQ wraps to 1, never back to 0.
            s.seq = s.seq.checked_add(1).unwrap_or(1);
            s.push(seq, &body)
        });
    }
}

/// A subscriber's delivery thread: events in order until the subscription
/// goes away or keeps failing.
fn delivery_loop(callbacks: Vec<String>, sid: String, rx: Receiver<Event>, dead: Arc<AtomicBool>) {
    let mut failures = 0;
    while let Ok((seq, body)) = rx.recv() {
        if deliver(&callbacks, &sid, seq, &body) {
            failures = 0;
        } else {
            failures += 1;
            if failures >= MAX_FAILURES {
                tracing::debug!("GENA subscriber {sid} unreachable: dropped");
                dead.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

/// Try each callback URL in order until one accepts; one retry round,
/// all within `DELIVERY_BUDGET`.
fn deliver(callbacks: &[String], sid: &str, seq: u32, body: &str) -> bool {
    let deadline = Instant::now() + DELIVERY_BUDGET;
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        for cb in callbacks {
            if Instant::now() >= deadline {
                return false;
            }
            if post_event(cb, sid, seq, body, deadline).is_some() {
                return true;
            }
        }
    }
    false
}

fn post_event(callback: &str, sid: &str, seq: u32, body: &str, deadline: Instant) -> Option<()> {
    let (host, path) = split_url(callback)?;
    let addr: SocketAddr = host.parse().ok()?;
    let left = || {
        let d = deadline.saturating_duration_since(Instant::now());
        (!d.is_zero()).then_some(d)
    };
    let req = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {host}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect_timeout(&addr, left()?.min(CONNECT_TIMEOUT)).ok()?;
    stream
        .set_write_timeout(Some(left()?.min(IO_TIMEOUT)))
        .ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    stream
        .set_read_timeout(Some(left()?.min(IO_TIMEOUT)))
        .ok()?;
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]);
    let status = resp.split_whitespace().nth(1).unwrap_or("");
    status.starts_with('2').then_some(())
}

fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let host = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    Some((host, path))
}

/// `<e:propertyset>` carrying each variable in its own `<e:property>`.
pub fn propertyset(vars: &[(&str, String)]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">",
    );
    for (k, v) in vars {
        s.push_str(&format!(
            "<e:property><{k}>{}</{k}></e:property>",
            xml::escape(v)
        ));
    }
    s.push_str("</e:propertyset>");
    s
}

/// A `LastChange` document (AVTransport / RenderingControl), unescaped;
/// `channel` adds `channel="Master"` (RenderingControl variables).
pub fn last_change(meta_ns: &str, vars: &[(&str, String)], channel: bool) -> String {
    let mut s = format!("<Event xmlns=\"{meta_ns}\"><InstanceID val=\"0\">");
    for (k, v) in vars {
        if channel {
            s.push_str(&format!(
                "<{k} channel=\"Master\" val=\"{}\"/>",
                xml::escape(v)
            ));
        } else {
            s.push_str(&format!("<{k} val=\"{}\"/>", xml::escape(v)));
        }
    }
    s.push_str("</InstanceID></Event>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_and_urls() {
        let peer: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(
            parse_callbacks(
                "<http://1.2.3.4:5/a><http://1.2.3.4/b> <ftp://1.2.3.4/x>",
                peer
            ),
            ["http://1.2.3.4:5/a", "http://1.2.3.4/b"]
        );
        // Only the subscriber itself, by address, with a real port.
        assert!(parse_callbacks("<http://5.6.7.8:5/a>", peer).is_empty());
        assert!(parse_callbacks("<http://example.org:5/a>", peer).is_empty());
        assert!(parse_callbacks("<http://1.2.3.4:0/a>", peer).is_empty());
        assert!(parse_callbacks("<https://1.2.3.4:5/a>", peer).is_empty());
        assert!(parse_callbacks("<http://1.2.3.4:99999/a>", peer).is_empty());
        let many = "<http://1.2.3.4:5/a>".repeat(10);
        assert_eq!(parse_callbacks(&many, peer).len(), MAX_CALLBACKS);
        assert_eq!(
            split_url("http://1.2.3.4:5/a/b").unwrap(),
            ("1.2.3.4:5".to_string(), "/a/b".to_string())
        );
        assert_eq!(split_url("http://h").unwrap().0, "h:80");
    }

    /// A callback that accepts connections and never answers.
    fn black_hole() -> (String, std::net::TcpListener) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        (format!("http://{}/cb", l.local_addr().unwrap()), l)
    }

    /// A callback that answers 200 and reports each SEQ it gets.
    fn good_callback() -> (String, mpsc::Receiver<u32>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/cb", l.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                s.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                let seq = head
                    .lines()
                    .find_map(|l| l.strip_prefix("SEQ: "))
                    .and_then(|v| v.trim().parse().ok());
                if let Some(seq) = seq {
                    let _ = tx.send(seq);
                }
            }
        });
        (url, rx)
    }

    #[test]
    fn dead_subscriber_stalls_nobody() {
        let subs = Subscribers::default();
        let (hole, _keep) = black_hole();
        let (good, rx) = good_callback();
        let s1 = subs.subscribe(vec![hole], 300).unwrap();
        let s2 = subs.subscribe(vec![good], 300).unwrap();
        let t = Instant::now();
        subs.initial(&s1, "<a/>");
        subs.initial(&s2, "<a/>");
        subs.notify("<b/>");
        subs.notify("<c/>");
        assert!(t.elapsed() < Duration::from_millis(200), "notify blocked");
        let got: Vec<u32> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(3)).expect("event"))
            .collect();
        assert_eq!(got, [0, 1, 2]);
    }

    #[test]
    fn failing_subscriber_is_dropped() {
        // A port nothing listens on: every delivery fails at once.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let subs = Subscribers::default();
        let sid = subs
            .subscribe(vec![format!("http://127.0.0.1:{port}/cb")], 300)
            .unwrap();
        subs.initial(&sid, "<a/>");
        subs.notify("<b/>");
        subs.notify("<c/>");
        let deadline = Instant::now() + Duration::from_secs(5);
        while subs.count() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            subs.expire();
        }
        assert_eq!(subs.count(), 0);
        assert!(!subs.renew(&sid, 300));
    }

    #[test]
    fn full_backlog_drops_the_subscriber() {
        let subs = Subscribers::default();
        let (hole, _keep) = black_hole();
        let sid = subs.subscribe(vec![hole], 300).unwrap();
        subs.initial(&sid, "<a/>");
        for _ in 0..MAX_BACKLOG + 2 {
            subs.notify("<b/>");
        }
        assert_eq!(subs.count(), 0);
    }

    #[test]
    fn documents_escape_values() {
        let p = propertyset(&[("Metatext", "a<b".into())]);
        assert!(p.contains("<e:property><Metatext>a&lt;b</Metatext></e:property>"));
        let lc = last_change(
            "urn:schemas-upnp-org:metadata-1-0/AVT/",
            &[("TransportState", "PLAYING".into())],
            false,
        );
        assert!(lc.contains("<TransportState val=\"PLAYING\"/>"));
        // LastChange travels escaped inside the property set.
        let p = propertyset(&[("LastChange", lc)]);
        assert!(p.contains("&lt;TransportState val=&quot;PLAYING&quot;/&gt;"));
    }
}
