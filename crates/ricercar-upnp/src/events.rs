use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Sub {
    callback: String,
    sid: String,
    seq: u32,
    expires: Instant,
}

/// One GENA subscription list (per evented service).
#[derive(Default)]
pub struct Subscribers {
    subs: Mutex<Vec<Sub>>,
    next_sid: AtomicU32,
}

impl Subscribers {
    pub fn subscribe(&self, callback: &str, timeout_secs: u64) -> (String, u64) {
        let n = self.next_sid.fetch_add(1, Ordering::Relaxed) + 1;
        let sid = format!("uuid:upnp-event-{}", n);
        {
            let mut subs = self.subs.lock().unwrap();
            subs.retain(|s| s.expires > Instant::now());
            subs.push(Sub {
                callback: callback.to_string(),
                sid: sid.clone(),
                seq: 0,
                expires: Instant::now() + Duration::from_secs(timeout_secs),
            });
        }
        (sid, timeout_secs)
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

    /// POST an EVENT to every subscriber, retrying each once (UPnP requires
    /// exactly one retry on failure).
    pub fn notify(&self, body: &str) {
        let targets: Vec<(String, String, u32)> = {
            let mut subs = self.subs.lock().unwrap();
            subs.retain(|s| s.expires > Instant::now());
            let out = subs
                .iter()
                .map(|s| (s.callback.clone(), s.sid.clone(), s.seq + 1))
                .collect();
            for s in subs.iter_mut() {
                s.seq += 1;
            }
            out
        };
        for (cb, sid, seq) in targets {
            if post_event(&cb, &sid, seq, body).is_none() {
                std::thread::sleep(Duration::from_millis(30));
                let _ = post_event(&cb, &sid, seq, body);
            }
        }
    }
}

fn post_event(callback: &str, sid: &str, seq: u32, body: &str) -> Option<()> {
    let (host, path) = split_url(callback)?;
    let req = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {host}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = std::net::TcpStream::connect(&host).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    use std::io::{Read, Write};
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]).into_owned();
    if resp.contains("200") { Some(()) } else { None }
}

fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/')?;
    Some((host.to_string(), format!("/{path}")))
}
