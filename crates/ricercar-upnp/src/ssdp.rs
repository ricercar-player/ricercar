use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SSDP: &str = "239.2.55.52:1900";
const CACHE_SECS: u32 = 1800;

pub struct Ssdp {
    sock: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
}

fn target() -> SocketAddr {
    SSDP.parse().unwrap()
}

fn advertise_types(udn: &str) -> Vec<(String, String)> {
    let host = format!("upnp://{}", udn);
    vec![
        ("upnp:rootdevice".into(), format!("upnp:rootdevice::{host}")),
        (format!("uuid:{udn}"), format!("uuid:{udn}")),
        (
            "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            format!("urn:schemas-upnp-org:device:MediaRenderer:1::{host}"),
        ),
        (
            "urn:schemas-upnp-org:service:AVTransport:1".into(),
            format!("urn:schemas-upnp-org:service:AVTransport:1::{host}"),
        ),
        (
            "urn:schemas-upnp-org:service:RenderingControl:1".into(),
            format!("urn:schemas-upnp-org:service:RenderingControl:1::{host}"),
        ),
        (
            "urn:schemas-upnp-org:service:ConnectionManager:1".into(),
            format!("urn:schemas-upnp-org:service:ConnectionManager:1::{host}"),
        ),
    ]
}

fn alive_packet(nt: &str, usn: &str, location: &str, bootid: u64) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\nHOST: {SSDP}\r\nCACHE-CONTROL: max-age={CACHE_SECS}\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nSERVER: Linux/5.0 UPnP/1.1 ricercar/0.1\r\nUSN: {usn}\r\nBOOTID.UPNP.ORG: {bootid}\r\nCONFIGID.UPNP.ORG: 1\r\n\r\n"
    )
}

fn byebye_packet(nt: &str, usn: &str, bootid: u64) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\nHOST: {SSDP}\r\nNT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {usn}\r\nBOOTID.UPNP.ORG: {bootid}\r\n\r\n"
    )
}

fn search_reply(
    st: &str,
    usn: &str,
    location: &str,
    bootid: u64,
) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={CACHE_SECS}\r\nSERVER: Linux/5.0 UPnP/1.1 ricercar/0.1\r\nST: {st}\r\nUSN: {usn}\r\nEXT:\r\nLOCATION: {location}\r\nBOOTID.UPNP.ORG: {bootid}\r\nCONFIGID.UPNP.ORG: 1\r\nCONTENT-LENGTH: 0\r\n\r\n"
    )
}

impl Ssdp {
    /// Binds UDP 1900 (reuse), joins the multicast group and spawns responder
    /// + alive threads. Returns None if the port is unavailable.
    pub fn start(udn: String, location: String) -> Option<Ssdp> {
        let sock = bind_ssdp_socket()?;
        let stop = Arc::new(AtomicBool::new(false));
        let bootid: u64 = std::process::id() as u64 * 1000;

        let sock_a = Arc::clone(&sock);
        let stop_a = stop.clone();
        let udn_a = udn.clone();
        let loc_a = location.clone();
        std::thread::Builder::new()
            .name("ricercar-ssdp-alive".into())
            .spawn(move || {
                let types = advertise_types(&udn_a);
                // three quick alives at boot, then periodic refresh
                let mut delays = [50u64, 150, 600];
                let mut i = 0usize;
                loop {
                    for (nt, usn) in &types {
                        let pkt = alive_packet(nt, usn, &loc_a, bootid);
                        let _ = sock_a.send_to(pkt.as_bytes(), target());
                    }
                    let wait = if i < delays.len() {
                        delays[i]
                    } else {
                        CACHE_SECS as u64 / 2 * 1000
                    };
                    i += 1;
                    let slept = std::time::Instant::now();
                    while slept.elapsed() < Duration::from_millis(wait) {
                        if stop_a.load(Ordering::Relaxed) {
                            for (nt, usn) in &types {
                                let pkt = byebye_packet(nt, usn, bootid);
                                let _ = sock_a.send_to(pkt.as_bytes(), target());
                            }
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            })
            .ok()?;

        let sock_r = Arc::clone(&sock);
        let stop_r = stop.clone();
        let udn_r = udn.clone();
        let loc_r = location.clone();
        std::thread::Builder::new()
            .name("ricercar-ssdp-resp".into())
            .spawn(move || {
                let types = advertise_types(&udn_r);
                let mut buf = [0u8; 2048];
                sock_r
                    .set_read_timeout(Some(Duration::from_millis(400)))
                    .ok();
                loop {
                    if stop_r.load(Ordering::Relaxed) {
                        break;
                    }
                    let (n, from) = match sock_r.recv_from(&mut buf) {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => continue,
                    };
                    let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if !msg.starts_with("M-SEARCH") {
                        continue;
                    }
                    let mut st = None;
                    for line in msg.lines() {
                        let lower = line.to_ascii_lowercase();
                        if let Some(rest) = lower.strip_prefix("st:") {
                            st = Some(rest.trim().to_string());
                        }
                    }
                    let Some(st) = st else { continue };
                    let mut out: Vec<(String, String)> = Vec::new();
                    for (nt, usn) in &types {
                        if st == "ssdp:all" || &st == nt {
                            out.push((nt.clone(), usn.clone()));
                        }
                    }
                    for (nt, usn) in out {
                        let pkt = search_reply(&nt, &usn, &loc_r, bootid);
                        let _ = sock_r.send_to(pkt.as_bytes(), from);
                    }
                }
            })
            .ok()?;

        Some(Ssdp { sock, stop })
    }
}

impl Drop for Ssdp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn bind_ssdp_socket() -> Option<Arc<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    socket.set_reuse_address(true).ok()?;
    socket
        .bind(&SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 1900).into())
        .ok()?;
    let sock: UdpSocket = socket.into();
    let iface = match local_ip() {
        IpAddr::V4(v) => v,
        _ => std::net::Ipv4Addr::UNSPECIFIED,
    };
    let _ = sock.join_multicast_v4(&"239.2.55.52".parse().unwrap(), &iface);
    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_multicast_ttl_v4(4);
    Some(Arc::new(sock))
}

pub fn local_ip() -> IpAddr {
    if_addrs::get_if_addrs()
        .map(|ifaces| {
            ifaces
                .iter()
                .find(|i| i.ip().is_ipv4() && !i.is_loopback())
                .map(|i| i.ip())
                .unwrap_or_else(|| IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        })
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}
