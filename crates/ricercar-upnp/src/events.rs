//! GENA eventing: subscriptions, property sets, NOTIFY delivery.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::xml;

/// Consecutive delivery failures after which a subscriber is dropped.
const MAX_FAILURES: u32 = 3;
/// Subscriptions per service (a misbehaving control point can't grow us).
const MAX_SUBS: usize = 32;

#[derive(Debug)]
struct Sub {
    callbacks: Vec<String>,
    sid: String,
    /// SEQ of the next event.
    seq: u32,
    expires: Instant,
    /// The initial event (SEQ 0) was sent: regular events may follow.
    ready: bool,
    failures: u32,
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

/// Parse a GENA `CALLBACK` header: one or more `<http://…>` URLs.
pub fn parse_callbacks(h: &str) -> Vec<String> {
    h.split('<')
        .filter_map(|p| p.split_once('>').map(|(u, _)| u.trim().to_string()))
        .filter(|u| u.starts_with("http://"))
        .collect()
}

impl Subscribers {
    /// New subscription; `None` when the list is full.
    pub fn subscribe(&self, callbacks: Vec<String>, timeout_secs: u64) -> Option<String> {
        let sid = new_sid();
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.expires > Instant::now());
        if subs.len() >= MAX_SUBS {
            return None;
        }
        subs.push(Sub {
            callbacks,
            sid: sid.clone(),
            seq: 0,
            expires: Instant::now() + Duration::from_secs(timeout_secs),
            ready: false,
            failures: 0,
        });
        Some(sid)
    }

    pub fn renew(&self, sid: &str, timeout_secs: u64) -> bool {
        let mut subs = self.subs.lock().unwrap();
        for s in subs.iter_mut() {
            if s.sid == sid {
                s.expires = Instant::now() + Duration::from_secs(timeout_secs);
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

    pub fn expire(&self) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.expires > Instant::now());
    }

    pub fn count(&self) -> usize {
        self.subs.lock().unwrap().len()
    }

    /// Send the initial event (SEQ 0) to one new subscriber.
    pub fn initial(&self, sid: &str, body: &str) {
        let target = {
            let mut subs = self.subs.lock().unwrap();
            let Some(s) = subs.iter_mut().find(|s| s.sid == sid && !s.ready) else {
                return;
            };
            s.ready = true;
            s.seq = 1;
            s.callbacks.clone()
        };
        let ok = deliver(&target, sid, 0, body);
        self.record(sid, ok);
    }

    /// POST an event to every ready subscriber, retrying once.
    pub fn notify(&self, body: &str) {
        let targets: Vec<(Vec<String>, String, u32)> = {
            let mut subs = self.subs.lock().unwrap();
            subs.retain(|s| s.expires > Instant::now());
            subs.iter_mut()
                .filter(|s| s.ready)
                .map(|s| {
                    let seq = s.seq;
                    // SEQ wraps to 1, never back to 0.
                    s.seq = s.seq.checked_add(1).unwrap_or(1);
                    (s.callbacks.clone(), s.sid.clone(), seq)
                })
                .collect()
        };
        for (cbs, sid, seq) in targets {
            let ok = deliver(&cbs, &sid, seq, body);
            self.record(&sid, ok);
        }
    }

    fn record(&self, sid: &str, ok: bool) {
        let mut subs = self.subs.lock().unwrap();
        if let Some(s) = subs.iter_mut().find(|s| s.sid == sid) {
            s.failures = if ok { 0 } else { s.failures + 1 };
        }
        subs.retain(|s| s.failures < MAX_FAILURES);
    }
}

/// Try each callback URL in order until one accepts; one retry round.
fn deliver(callbacks: &[String], sid: &str, seq: u32, body: &str) -> bool {
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if callbacks
            .iter()
            .any(|cb| post_event(cb, sid, seq, body).is_some())
        {
            return true;
        }
    }
    false
}

fn post_event(callback: &str, sid: &str, seq: u32, body: &str) -> Option<()> {
    let (host, path) = split_url(callback)?;
    let req = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {host}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}",
        body.len()
    );
    let addr = host.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(1000)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(2000)))
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(2000)))
        .ok()?;
    stream.write_all(req.as_bytes()).ok()?;
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
        assert_eq!(
            parse_callbacks("<http://1.2.3.4:5/a><http://h/b> <ftp://x>"),
            ["http://1.2.3.4:5/a", "http://h/b"]
        );
        assert_eq!(
            split_url("http://1.2.3.4:5/a/b").unwrap(),
            ("1.2.3.4:5".to_string(), "/a/b".to_string())
        );
        assert_eq!(split_url("http://h").unwrap().0, "h:80");
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
