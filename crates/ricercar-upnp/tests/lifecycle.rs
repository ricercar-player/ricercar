//! Starting, stopping and restarting the network services (settings applied
//! without restarting the app).

mod common;

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use common::get;
use ricercar_core::{Controller, Library};
use ricercar_upnp::UpnpOptions;

fn controller() -> Arc<Controller> {
    let lib = Arc::new(Library::in_memory().unwrap());
    Arc::new(Controller::new(lib, "null"))
}

fn opts(name: &str, renderer: bool, media_server: bool, port: u16) -> UpnpOptions {
    UpnpOptions {
        name: name.into(),
        renderer,
        media_server,
        port,
    }
}

fn status(head: &str) -> &str {
    head.split_whitespace().nth(1).unwrap_or("")
}

#[test]
fn restart_with_another_name() {
    let ctl = controller();
    let mut h = ricercar_upnp::start(ctl.clone(), &opts("Before", true, true, 0)).unwrap();
    let port = h.port;
    let (head, body) = get(port, "/device.xml", "");
    assert_eq!(status(&head), "200");
    let before = String::from_utf8_lossy(&body).into_owned();
    assert!(before.contains("<friendlyName>Before</friendlyName>"));

    h.stop();
    assert!(
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300))
            .is_err(),
        "port still open after stop"
    );

    // Same port asked again: control points keep their cached URLs.
    let h = ricercar_upnp::start(ctl, &opts("After", true, true, port)).unwrap();
    assert_eq!(h.port, port);
    let (_, body) = get(h.port, "/device.xml", "");
    let after = String::from_utf8_lossy(&body).into_owned();
    assert!(after.contains("<friendlyName>After</friendlyName>"));
    // Same device identity.
    let udn = |xml: &str| {
        xml.split("<UDN>")
            .nth(1)
            .map(|s| s[..s.find('<').unwrap()].to_string())
    };
    assert_eq!(udn(&before), udn(&after));
}

#[test]
fn server_without_renderer() {
    let h = ricercar_upnp::start(controller(), &opts("Shelf", false, true, 0)).unwrap();
    let (head, _) = get(h.port, "/device.xml", "");
    assert_eq!(status(&head), "404");
    let (head, _) = get(h.port, "/svc/avt.xml", "");
    assert_eq!(status(&head), "404");
    let (head, body) = get(h.port, "/server.xml", "");
    assert_eq!(status(&head), "200");
    assert!(
        String::from_utf8_lossy(&body).contains("<friendlyName>Shelf (library)</friendlyName>")
    );
}

#[test]
fn renderer_without_server() {
    let h = ricercar_upnp::start(controller(), &opts("Player", true, false, 0)).unwrap();
    let (head, _) = get(h.port, "/device.xml", "");
    assert_eq!(status(&head), "200");
    let (head, _) = get(h.port, "/server.xml", "");
    assert_eq!(status(&head), "404");
}
