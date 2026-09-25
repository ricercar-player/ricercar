//! SSDP discovery: multicast announcements on every IPv4 interface,
//! M-SEARCH replies after the MX-random delay, `ssdp:byebye` on shutdown.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
const CACHE_SECS: u32 = 1800;
/// M-SEARCH replies waiting for their delay; more are answered at once
/// (bounded thread count under a search flood).
const MAX_PENDING_REPLIES: usize = 16;
const SERVER: &str = "Linux/6 UPnP/1.1 ricercar/0.2";

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Renderer,
    Server,
}

#[derive(Clone)]
pub struct Device {
    pub udn: String,
    /// Description path on our HTTP server (`/device.xml`).
    pub path: &'static str,
    pub kind: Kind,
}

/// (NT/ST, USN, description path)
type Entry = (String, String, &'static str);

fn device_entries(dev: &Device) -> Vec<Entry> {
    let uuid = format!("uuid:{}", dev.udn);
    let mut nts: Vec<String> = vec!["upnp:rootdevice".into(), uuid.clone()];
    match dev.kind {
        Kind::Renderer => {
            nts.push("urn:schemas-upnp-org:device:MediaRenderer:1".into());
            for svc in ["AVTransport", "RenderingControl", "ConnectionManager"] {
                nts.push(format!("urn:schemas-upnp-org:service:{svc}:1"));
            }
            for svc in ["Product", "Playlist", "Info", "Time", "Volume"] {
                nts.push(format!("urn:av-openhome-org:service:{svc}:1"));
            }
        }
        Kind::Server => {
            nts.push("urn:schemas-upnp-org:device:MediaServer:1".into());
            for svc in ["ContentDirectory", "ConnectionManager"] {
                nts.push(format!("urn:schemas-upnp-org:service:{svc}:1"));
            }
        }
    }
    nts.into_iter()
        .map(|nt| {
            let usn = if nt == uuid {
                uuid.clone()
            } else {
                format!("{uuid}::{nt}")
            };
            (nt, usn, dev.path)
        })
        .collect()
}

struct Common {
    entries: Vec<Entry>,
    port: u16,
    bootid: u32,
}

impl Common {
    fn location(&self, ip: Ipv4Addr, path: &str) -> String {
        format!("http://{ip}:{}{path}", self.port)
    }

    fn alive(&self, e: &Entry, ip: Ipv4Addr) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {GROUP}:{PORT}\r\nCACHE-CONTROL: max-age={CACHE_SECS}\r\nLOCATION: {}\r\nNT: {}\r\nNTS: ssdp:alive\r\nSERVER: {SERVER}\r\nUSN: {}\r\nBOOTID.UPNP.ORG: {}\r\nCONFIGID.UPNP.ORG: 1\r\n\r\n",
            self.location(ip, e.2),
            e.0,
            e.1,
            self.bootid
        )
    }

    fn byebye(&self, e: &Entry) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {GROUP}:{PORT}\r\nNT: {}\r\nNTS: ssdp:byebye\r\nUSN: {}\r\nBOOTID.UPNP.ORG: {}\r\nCONFIGID.UPNP.ORG: 1\r\n\r\n",
            e.0, e.1, self.bootid
        )
    }

    fn reply(&self, st: &str, e: &Entry, ip: Ipv4Addr) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={CACHE_SECS}\r\nDATE: {}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {SERVER}\r\nST: {st}\r\nUSN: {}\r\nBOOTID.UPNP.ORG: {}\r\nCONFIGID.UPNP.ORG: 1\r\nCONTENT-LENGTH: 0\r\n\r\n",
            http_date(),
            self.location(ip, e.2),
            e.1,
            self.bootid
        )
    }

    /// Send alive (or byebye) for every entry on every interface.
    fn announce(&self, sock: &socket2::Socket, ifaces: &[Iface], alive: bool) {
        let dst = socket2::SockAddr::from(SocketAddr::new(IpAddr::V4(GROUP), PORT));
        for i in ifaces {
            let _ = sock.set_multicast_if_v4(&i.ip);
            for e in &self.entries {
                let pkt = if alive {
                    self.alive(e, i.ip)
                } else {
                    self.byebye(e)
                };
                let _ = sock.send_to(pkt.as_bytes(), &dst);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Iface {
    ip: Ipv4Addr,
    mask: Ipv4Addr,
}

fn ifaces() -> Vec<Iface> {
    let mut v: Vec<Iface> = if_addrs::get_if_addrs()
        .map(|l| {
            l.into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.addr {
                    if_addrs::IfAddr::V4(a) => Some(Iface {
                        ip: a.ip,
                        mask: a.netmask,
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|i| i.ip);
    v.dedup();
    v
}

/// The local address a peer should use to reach us: same subnet first.
fn ip_for_peer(peer: IpAddr, ifs: &[Iface]) -> Ipv4Addr {
    let IpAddr::V4(p) = peer else {
        return ifs.first().map(|i| i.ip).unwrap_or(Ipv4Addr::LOCALHOST);
    };
    if p.is_loopback() {
        return Ipv4Addr::LOCALHOST;
    }
    let same = |i: &&Iface| {
        let m = u32::from(i.mask);
        u32::from(i.ip) & m == u32::from(p) & m
    };
    ifs.iter()
        .find(same)
        .or(ifs.first())
        .map(|i| i.ip)
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

fn http_date() -> String {
    // RFC 1123 date, computed without a time crate.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem / 60) % 60, rem % 60);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    // civil_from_days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if mo <= 2 { 1 } else { 0 };
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(mo - 1) as usize];
    format!("{wd}, {d:02} {mon} {y} {h:02}:{m:02}:{s:02} GMT")
}

/// Small non-cryptographic random number (reply jitter only).
fn jitter_ms(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x % max
}

/// Headers of an M-SEARCH we answer: (ST, MX seconds, multicast).
fn parse_msearch(msg: &str) -> Option<(String, u64, bool)> {
    let mut lines = msg.lines();
    if !lines.next()?.starts_with("M-SEARCH") {
        return None;
    }
    let (mut st, mut mx, mut man, mut multicast) = (None, None, false, false);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "st" => st = Some(v.to_string()),
            "mx" => mx = v.parse::<u64>().ok(),
            "man" => man = v.trim_matches('"') == "ssdp:discover",
            "host" => multicast = v.starts_with("239.255.255.250"),
            _ => {}
        }
    }
    if !man {
        return None;
    }
    // Multicast searches must carry MX (1..=5 used); unicast answer at once.
    let mx = if multicast { mx?.clamp(1, 5) } else { 0 };
    Some((st?, mx, multicast))
}

pub struct Ssdp {
    sock: Arc<socket2::Socket>,
    common: Arc<Common>,
    stop: Arc<AtomicBool>,
}

impl Ssdp {
    /// Binds UDP 1900 (reuse), joins the group on every interface and spawns
    /// responder + announcer threads. `None` if the port is unavailable.
    pub fn start(port: u16, devices: Vec<Device>) -> Option<Ssdp> {
        let sock = bind_ssdp_socket()?;
        let send = Arc::new(sender_socket()?);
        let stop = Arc::new(AtomicBool::new(false));
        let bootid = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(1)
            & 0x7fff_ffff;
        let common = Arc::new(Common {
            entries: devices.iter().flat_map(device_entries).collect(),
            port,
            bootid,
        });

        // Announcer: burst at boot, refresh every CACHE/2, re-announce when
        // the set of interfaces changes.
        {
            let (common, send, sock, stop) =
                (common.clone(), send.clone(), sock.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ricercar-ssdp-alive".into())
                .spawn(move || {
                    let mut known = ifaces();
                    join_all(&sock, &known);
                    let mut burst = [50u64, 150, 600].into_iter();
                    let mut next = Instant::now();
                    let mut last_check = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        if Instant::now() >= next {
                            common.announce(&send, &known, true);
                            next = Instant::now()
                                + Duration::from_millis(
                                    burst.next().unwrap_or(CACHE_SECS as u64 / 2 * 1000),
                                );
                        }
                        if last_check.elapsed() >= Duration::from_secs(5) {
                            last_check = Instant::now();
                            let now = ifaces();
                            if now != known {
                                tracing::info!("network interfaces changed: re-announcing");
                                join_all(&sock, &now);
                                known = now;
                                burst = [50u64, 150, 600].into_iter();
                                next = Instant::now();
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .ok()?;
        }

        // Responder.
        {
            let (common, sock, stop) = (common.clone(), sock.clone(), stop.clone());
            let pending = Arc::new(AtomicUsize::new(0));
            std::thread::Builder::new()
                .name("ricercar-ssdp-resp".into())
                .spawn(move || {
                    let mut buf = [0u8; 2048];
                    sock.set_read_timeout(Some(Duration::from_millis(300))).ok();
                    while !stop.load(Ordering::Relaxed) {
                        let Ok((n, from)) = sock.recv_from(&mut buf) else {
                            continue;
                        };
                        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let Some((st, mx, _)) = parse_msearch(&msg) else {
                            continue;
                        };
                        let ip = ip_for_peer(from.ip(), &ifaces());
                        let replies: Vec<String> = common
                            .entries
                            .iter()
                            .filter(|e| st == "ssdp:all" || st == e.0)
                            .map(|e| common.reply(&e.0, e, ip))
                            .collect();
                        if replies.is_empty() {
                            continue;
                        }
                        let delay = jitter_ms(mx * 1000);
                        if delay == 0 || pending.load(Ordering::Relaxed) >= MAX_PENDING_REPLIES {
                            for r in &replies {
                                let _ = sock.send_to(r.as_bytes(), from);
                            }
                            continue;
                        }
                        pending.fetch_add(1, Ordering::Relaxed);
                        let (sock2, pending2) = (sock.clone(), pending.clone());
                        let spawned = std::thread::Builder::new()
                            .name("ricercar-ssdp-reply".into())
                            .spawn(move || {
                                std::thread::sleep(Duration::from_millis(delay));
                                for r in &replies {
                                    let _ = sock2.send_to(r.as_bytes(), from);
                                }
                                pending2.fetch_sub(1, Ordering::Relaxed);
                            });
                        if spawned.is_err() {
                            pending.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                })
                .ok()?;
        }

        Some(Ssdp {
            sock: send,
            common,
            stop,
        })
    }
}

impl Drop for Ssdp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.common.announce(&self.sock, &ifaces(), false);
    }
}

fn join_all(sock: &UdpSocket, ifs: &[Iface]) {
    for i in ifs {
        // Already-joined interfaces return an error: harmless.
        let _ = sock.join_multicast_v4(&GROUP, &i.ip);
    }
    if ifs.is_empty() {
        let _ = sock.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED);
    }
}

/// Socket for multicast announcements (outgoing interface set per send).
fn sender_socket() -> Option<socket2::Socket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    s.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into())
        .ok()?;
    let _ = s.set_multicast_ttl_v4(4);
    let _ = s.set_multicast_loop_v4(true);
    Some(s)
}

fn bind_ssdp_socket() -> Option<Arc<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    socket.set_reuse_address(true).ok()?;
    socket
        .bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT).into())
        .ok()?;
    let sock: UdpSocket = socket.into();
    let _ = sock.set_multicast_loop_v4(true);
    Some(Arc::new(sock))
}

/// Primary LAN address (URLs emitted outside of a request).
pub fn local_ip() -> IpAddr {
    ifaces()
        .first()
        .map(|i| IpAddr::V4(i.ip))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_choice() {
        let ifs = [
            Iface {
                ip: Ipv4Addr::new(10, 0, 0, 5),
                mask: Ipv4Addr::new(255, 255, 255, 0),
            },
            Iface {
                ip: Ipv4Addr::new(192, 168, 1, 7),
                mask: Ipv4Addr::new(255, 255, 255, 0),
            },
        ];
        assert_eq!(
            ip_for_peer("192.168.1.42".parse().unwrap(), &ifs),
            Ipv4Addr::new(192, 168, 1, 7)
        );
        assert_eq!(
            ip_for_peer("172.16.0.1".parse().unwrap(), &ifs),
            Ipv4Addr::new(10, 0, 0, 5)
        );
        assert_eq!(
            ip_for_peer("127.0.0.1".parse().unwrap(), &ifs),
            Ipv4Addr::LOCALHOST
        );
    }

    #[test]
    fn msearch_parsing() {
        let m = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 9\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(m), Some(("ssdp:all".into(), 5, true)));
        let no_mx = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(parse_msearch(no_mx), None);
        let unicast = "M-SEARCH * HTTP/1.1\r\nHOST: 10.0.0.5:1900\r\nMAN: \"ssdp:discover\"\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(
            parse_msearch(unicast),
            Some(("upnp:rootdevice".into(), 0, false))
        );
        assert!(jitter_ms(1000) < 1000);
    }

    #[test]
    fn entries_cover_openhome() {
        let e = device_entries(&Device {
            udn: "u".into(),
            path: "/device.xml",
            kind: Kind::Renderer,
        });
        assert!(
            e.iter()
                .any(|x| x.0 == "urn:av-openhome-org:service:Playlist:1")
        );
        assert!(e.iter().any(|x| x.1 == "uuid:u"));
        assert!(
            e.iter()
                .any(|x| x.1 == "uuid:u::urn:schemas-upnp-org:device:MediaRenderer:1")
        );
        assert!(http_date().ends_with(" GMT"));
    }
}
