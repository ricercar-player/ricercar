//! Serving on one chosen network interface (`[network] interface`).

mod common;

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::get;
use ricercar_core::{Controller, Library};
use ricercar_upnp::UpnpOptions;

fn controller() -> Arc<Controller> {
    let lib = Arc::new(Library::in_memory().unwrap());
    Arc::new(Controller::new(lib, "null"))
}

fn on(interface: &str, port: u16) -> UpnpOptions {
    UpnpOptions {
        name: "Bound".into(),
        renderer: true,
        media_server: true,
        port,
        interface: Some(interface.into()),
    }
}

fn reachable(ip: Ipv4Addr, port: u16) -> bool {
    TcpStream::connect_timeout(&(ip, port).into(), Duration::from_millis(300)).is_ok()
}

#[test]
fn bound_to_loopback_only() {
    let Some(lo) = ricercar_upnp::interface_ipv4("lo") else {
        eprintln!("no `lo` interface: skipped");
        return;
    };
    let mut h = ricercar_upnp::start(controller(), &on("lo", 0)).unwrap();
    assert_eq!(h.ip, Some(lo));
    let (head, body) = get(h.port, "/device.xml", "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(String::from_utf8_lossy(&body).contains("<friendlyName>Bound</friendlyName>"));

    // Not on the machine's other addresses (when it has any).
    for (name, ip) in ricercar_upnp::interfaces() {
        assert!(!reachable(ip, h.port), "reachable on {name} {ip}");
    }

    h.stop();
    assert!(!reachable(lo, h.port), "port still open after stop");
}

#[test]
fn unknown_interface_binds_nothing() {
    // A port that is free right now.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = ricercar_upnp::start(controller(), &on("no-such-if0", port))
        .err()
        .expect("started on a missing interface");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrNotAvailable);
    assert_eq!(err.to_string(), "Interface no-such-if0 is not available");
    assert!(!reachable(Ipv4Addr::LOCALHOST, port));
    assert!(TcpListener::bind(("0.0.0.0", port)).is_ok(), "port taken");
}

#[test]
fn empty_interface_means_all() {
    let h = ricercar_upnp::start(controller(), &on("", 0)).unwrap();
    assert_eq!(h.ip, None);
    let (head, _) = get(h.port, "/device.xml", "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}
